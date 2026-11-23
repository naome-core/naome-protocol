use super::{
    Archive, IndexedRecord, Passage, Snapshot,
    extract::{normalize, terms},
    text::VerifiedTextChunk,
};
use crate::{profile::Document, wire::Id};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::ops::Bound::{Excluded, Unbounded};

pub(super) const INDEX_VERSION: u32 = 5;
const BATCH: usize = 16;
const TEXT_BATCH: usize = 4;
const ADD_MEDIA_PASSAGE_INDEX: &str = "CREATE INDEX IF NOT EXISTS rep_search_media_passages
    ON rep_search_passages(entry) WHERE semantic AND text=''";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct State {
    cursor: Option<Id>,
    finished: bool,
}

enum Record {
    Cached { entries: Vec<i64>, limited: bool },
    Extracted(IndexedRecord),
}

impl Archive {
    pub(super) fn init_search(&self) -> Result<()> {
        if installed_version(&self.db)? == Some(INDEX_VERSION)
            && self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema
                    WHERE type='index' AND name='rep_search_media_passages')",
                [],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Ok(());
        }
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        match installed_version(&tx)? {
            Some(INDEX_VERSION) => {}
            Some(1) => {
                tx.execute_batch(include_str!("replace_v1.sql"))?;
                tx.execute_batch(include_str!("schema.sql"))?;
            }
            Some(2..=4) => {
                tx.execute_batch(include_str!("replace_v2_to_v4.sql"))?;
                tx.execute_batch(include_str!("schema.sql"))?;
            }
            None => tx.execute_batch(include_str!("schema.sql"))?,
            Some(_) => anyhow::bail!("unsupported local search index version"),
        }
        tx.execute_batch(ADD_MEDIA_PASSAGE_INDEX)?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn refresh_search(
        &self,
        snapshot: &Snapshot,
        head: &str,
        mut budget: usize,
    ) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO rep_search_heads(head) VALUES(?)",
            [head],
        )?;
        let mut state = state(&self.db, head)?.context("search snapshot missing")?;
        while budget > 0 {
            let revision = revision(&self.db)?;
            let count = budget.min(BATCH);
            let ready = ready_objects(&self.db, head, count)?;
            let advancing = ready.is_empty();
            let records = if advancing {
                if state.finished {
                    break;
                }
                let bound = state.cursor.map_or(Unbounded, Excluded);
                snapshot
                    .objects
                    .range((bound, Unbounded))
                    .copied()
                    .filter(|id| id.format().is_some())
                    .take(count + 1)
                    .collect()
            } else {
                ready
            };
            let finished = records.len() <= count;
            let indexed: Vec<_> = records
                .into_iter()
                .take(count)
                .map(|id| {
                    let record = cached_record(&self.db, snapshot, id)?
                        .unwrap_or_else(|| Record::Extracted(self.search_sections(snapshot, id)));
                    Ok((id, record))
                })
                .collect::<Result<_>>()?;
            let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
            if self::revision(&tx)? != revision || self::state(&tx, head)? != Some(state) {
                return Ok(());
            }
            for (id, record) in &indexed {
                publish(&tx, head, *id, record)?;
            }
            if advancing {
                state.cursor = indexed.last().map(|(id, _)| *id).or(state.cursor);
                state.finished = finished;
                tx.execute(
                    "UPDATE rep_search_heads SET cursor=?,finished=? WHERE head=?",
                    params![state.cursor.map(|id| id.to_string()), state.finished, head],
                )?;
            }
            tx.commit()?;
            budget -= indexed.len();
        }
        self.refresh_text(snapshot, head, budget.min(TEXT_BATCH))
    }

    fn refresh_text(&self, snapshot: &Snapshot, head: &str, budget: usize) -> Result<()> {
        let mut chunk = VerifiedTextChunk::default();
        let mut chunk_revision = None;
        for _ in 0..budget {
            let revision = revision(&self.db)?;
            // A source can be withdrawn between windows or before the publish transaction.
            if chunk_revision != Some(revision) {
                chunk = VerifiedTextChunk::default();
                chunk_revision = Some(revision);
            }
            let Some((entry, object, cursor, length)) = self
                .db
                .query_row(
                    "SELECT t.entry,e.object,t.cursor,t.length FROM rep_search_visibility v
                 JOIN rep_search_text t ON t.entry=v.entry
                 JOIN rep_search_entries e ON e.id=t.entry
                 WHERE v.head=? AND t.cursor<t.length ORDER BY e.object LIMIT 1",
                    [head],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()?
            else {
                break;
            };
            let indexed = self.index_text_window(
                snapshot,
                object.parse()?,
                usize::try_from(cursor)?,
                usize::try_from(length)?,
                &mut chunk,
            );
            let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
            if self::revision(&tx)? != revision {
                break;
            }
            let (passages, next, limited) = indexed?;
            let changed = tx.execute(
                "UPDATE rep_search_text SET cursor=?2 WHERE entry=?1 AND cursor=?3
                   AND EXISTS(SELECT 1 FROM rep_search_visibility WHERE head=?4 AND entry=?1)",
                params![entry, i64::try_from(next)?, cursor, head],
            )?;
            if changed != 0 {
                write_passages(&tx, entry, &passages)?;
                if limited {
                    tx.execute(
                        "UPDATE rep_search_entries SET coverage=1
                         WHERE object=? AND section=0 AND coverage IS NOT NULL",
                        [&object],
                    )?;
                    tx.execute(
                        "INSERT OR IGNORE INTO rep_search_gaps(head,object)
                         SELECT head,object FROM rep_search_visibility WHERE entry=?",
                        [entry],
                    )?;
                }
            }
            tx.commit()?;
        }
        Ok(())
    }
}

pub(super) fn prepared_cache_available(db: &Connection) -> Result<bool> {
    match installed_version(db)? {
        Some(INDEX_VERSION) => Ok(true),
        None | Some(1..=4) => Ok(false),
        Some(_) => anyhow::bail!("unsupported local search index version"),
    }
}

fn installed_version(db: &Connection) -> Result<Option<u32>> {
    let found: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='rep_search_schema')",
        [],
        |row| row.get(0),
    )?;
    Ok(if found {
        Some(
            db.query_row("SELECT version FROM rep_search_schema", [], |row| {
                row.get(0)
            })?,
        )
    } else {
        None
    })
}

fn revision(db: &Connection) -> Result<i64> {
    Ok(
        db.query_row("SELECT value FROM rep_search_revision", [], |row| {
            row.get(0)
        })?,
    )
}

fn state(db: &Connection, head: &str) -> Result<Option<State>> {
    db.query_row(
        "SELECT cursor,finished FROM rep_search_heads WHERE head=?",
        [head],
        |row| Ok((row.get::<_, Option<String>>(0)?, row.get(1)?)),
    )
    .optional()?
    .map(|(cursor, finished)| {
        Ok(State {
            cursor: cursor.map(|id| id.parse()).transpose()?,
            finished,
        })
    })
    .transpose()
}

pub(super) fn coverage(db: &Connection, head: &str) -> Result<(bool, bool)> {
    Ok(db.query_row(
        "SELECT coalesce((SELECT finished FROM rep_search_heads WHERE head=?1), 0)
                AND NOT EXISTS(SELECT 1 FROM rep_search_waiting WHERE head=?1 AND ready)
                AND NOT EXISTS(SELECT 1 FROM rep_search_text t
                    JOIN rep_search_visibility v ON v.entry=t.entry WHERE v.head=?1 AND t.cursor<t.length),
                EXISTS(SELECT 1 FROM rep_search_gaps WHERE head=?1)",
        [head],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

fn ready_objects(db: &Connection, head: &str, limit: usize) -> Result<Vec<Id>> {
    db.prepare_cached(
        "SELECT DISTINCT object FROM rep_search_waiting
         WHERE head=? AND ready ORDER BY object LIMIT ?",
    )?
    .query_map(params![head, limit as i64], |row| row.get::<_, String>(0))?
    .map(|row| row?.parse())
    .collect()
}

fn cached_record(db: &Connection, snapshot: &Snapshot, id: Id) -> Result<Option<Record>> {
    let key = id.to_string();
    let limited: Option<bool> = db.query_row(
        "SELECT coverage FROM rep_search_entries WHERE object=? AND section=0 AND coverage IS NOT NULL",
        [&key], |row| row.get(0),
    ).optional()?;
    let Some(limited) = limited else {
        return Ok(None);
    };
    let mut statement = db.prepare_cached(
        "SELECT e.id,s.source FROM rep_search_entries e JOIN rep_search_sources s ON s.entry=e.id
         WHERE e.object=? ORDER BY e.id",
    )?;
    let mut rows = statement.query([key])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        let id = row.get::<_, String>(1)?.parse()?;
        if !snapshot.objects.contains(&id) {
            return Ok(None);
        }
        let entry: i64 = row.get(0)?;
        if entries.last() != Some(&entry) {
            entries.push(entry);
        }
    }
    Ok(Some(Record::Cached { entries, limited }))
}

fn publish(db: &Connection, head: &str, id: Id, record: &Record) -> Result<()> {
    let key = id.to_string();
    db.execute(
        "DELETE FROM rep_search_visibility WHERE head=?1
         AND entry IN (SELECT id FROM rep_search_entries WHERE object=?2)",
        params![head, key],
    )?;
    db.execute(
        "DELETE FROM rep_search_waiting WHERE head=? AND object=?",
        params![head, key],
    )?;
    let (entries, limited) = match record {
        Record::Cached { entries, limited } => (entries.clone(), *limited),
        Record::Extracted(extracted) => {
            let entries = if let Some(document) = &extracted.document {
                write_sections(db, &key, document, extracted)?
            } else {
                Vec::new()
            };
            for missing in &extracted.missing {
                db.execute(
                    "INSERT OR IGNORE INTO rep_search_waiting(source,head,object) VALUES(?,?,?)",
                    params![missing.to_string(), head, key],
                )?;
            }
            (entries, extracted.limited)
        }
    };
    for entry in entries {
        db.execute(
            "INSERT OR IGNORE INTO rep_search_visibility(head,entry,object,section)
             SELECT ?1,id,object,section FROM rep_search_entries WHERE id=?2",
            params![head, entry],
        )?;
    }
    if limited {
        db.execute(
            "INSERT OR IGNORE INTO rep_search_gaps VALUES(?,?)",
            params![head, key],
        )?;
    } else {
        db.execute(
            "DELETE FROM rep_search_gaps WHERE head=? AND object=?",
            params![head, key],
        )?;
    }
    Ok(())
}

fn write_sections(
    db: &Connection,
    id: &str,
    document: &Document,
    record: &IndexedRecord,
) -> Result<Vec<i64>> {
    let mut entries = Vec::new();
    let mut lookup =
        db.prepare_cached("SELECT id FROM rep_search_entries WHERE object=? AND section=?")?;
    let mut insert = db.prepare_cached(
        "INSERT INTO rep_search_entries(object,section,kind,subject,scope) VALUES(?,?,?,?,?)",
    )?;
    let mut source = db.prepare_cached("INSERT INTO rep_search_sources VALUES(?,?)")?;
    for (section_number, section) in record.sections.iter().enumerate() {
        let section_number = section_number as i64;
        let existing = lookup
            .query_row(params![id, section_number], |row| row.get(0))
            .optional()?;
        if let Some(key) = existing {
            entries.push(key);
            continue;
        }
        ensure!(!section.sources.is_empty(), "search section has no source");
        let key = insert.insert(params![
            id,
            section_number,
            document.kind as i64,
            document.subject,
            document.scope
        ])?;
        entries.push(key);
        for source_id in &section.sources {
            source.execute(params![source_id.to_string(), key])?;
        }
        if let Some(length) = section.pending_text {
            db.execute(
                "INSERT INTO rep_search_text(entry,length) VALUES(?,?)",
                params![key, i64::try_from(length)?],
            )?;
        }
        write_passages(db, key, &section.passages)?;
    }
    if record.cacheable {
        db.execute(
            "UPDATE rep_search_entries SET coverage=? WHERE object=? AND section=0",
            params![record.limited, id],
        )?;
    }
    Ok(entries)
}

fn write_passages(db: &Connection, entry: i64, passages: &[Passage]) -> Result<()> {
    let mut row = db.prepare_cached(
        "INSERT INTO rep_search_passages(entry,location,text,normalized,priority,semantic) VALUES(?,?,?,?,?,?)",
    )?;
    let mut posting = db.prepare_cached("INSERT INTO rep_search_terms VALUES(?,?)")?;
    for passage in passages {
        let normalized = normalize(&passage.text);
        let location = serde_json::to_string(&passage.location)?;
        let id = row.insert(params![
            entry,
            location,
            passage.text,
            normalized,
            passage.priority,
            passage.semantic
        ])?;
        for term in terms(&normalized) {
            posting.execute(params![term, id])?;
        }
    }
    Ok(())
}
