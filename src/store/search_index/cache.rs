use super::{
    Archive, IndexedRecord, InventoryReader, Passage,
    lexical::{normalize, terms},
    text::{VerifiedTextChunk, index_text_window},
};
use crate::{profile::Document, wire::Id};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::BTreeSet;

pub(super) const INDEX_VERSION: u32 = 7;
const BATCH: usize = 16;
const TEXT_BATCH: usize = 4;
const ADD_MEDIA_PASSAGE_INDEX: &str = "CREATE INDEX IF NOT EXISTS rep_search_media_passages
    ON rep_search_passages(entry) WHERE semantic AND text=''";
const ADD_VECTOR_PROGRESS: &str = "CREATE TABLE IF NOT EXISTS rep_search_vector_progress(
    head TEXT NOT NULL REFERENCES rep_search_heads(head) ON DELETE CASCADE,
    model TEXT NOT NULL,
    cursor INTEGER NOT NULL,
    PRIMARY KEY(head,model)
) WITHOUT ROWID;";
const ADD_WITNESS_ARRIVAL: &str = "CREATE TRIGGER rep_search_witness_arrival
    AFTER INSERT ON custody_pages
    WHEN EXISTS(SELECT 1 FROM rep_search_waiting WHERE source='page/' || NEW.id AND NOT ready)
    BEGIN
        UPDATE rep_search_waiting SET ready=1 WHERE source='page/' || NEW.id;
        UPDATE rep_search_revision SET value=value+1;
    END;";

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
        if current_schema(&self.db)?
            && vector_progress_installed(&self.db)?
            && witness_arrival_installed(&self.db)?
        {
            return Ok(());
        }
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let previous = installed_version(&tx)?;
        match previous {
            Some(INDEX_VERSION) => {}
            Some(5 | 6) => tx.execute_batch(
                "DROP TABLE rep_search_schema;
                CREATE TABLE rep_search_schema(version INTEGER PRIMARY KEY CHECK(version=7));
                INSERT INTO rep_search_schema VALUES(7);",
            )?,
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
        if !matches!(previous, Some(6 | INDEX_VERSION)) {
            tx.execute_batch(include_str!("visibility.sql"))?;
            if previous == Some(5) {
                // Preserve extracted text and vectors while changing ownership of the mappings.
                tx.execute_batch(
                    "INSERT INTO rep_search_sets(writer) SELECT head FROM rep_search_heads;
                     INSERT INTO rep_search_head_sets SELECT writer,id FROM rep_search_sets;
                     INSERT INTO rep_search_set_entries SELECT s.id,v.entry,v.object,v.section
                         FROM rep_search_visibility v JOIN rep_search_sets s ON s.writer=v.head;
                     DROP TABLE rep_search_visibility;",
                )?;
            }
            tx.execute_batch(include_str!("positions.sql"))?;
        }
        if previous == Some(6) {
            tx.execute_batch(
                "DROP TRIGGER rep_search_sample_new_vector;
                              DROP TRIGGER rep_search_sample_lost_vector;",
            )?;
        }
        if previous != Some(INDEX_VERSION) {
            tx.execute_batch(include_str!("vector_visibility.sql"))?;
        }
        tx.execute_batch(ADD_MEDIA_PASSAGE_INDEX)?;
        tx.execute_batch(ADD_VECTOR_PROGRESS)?;
        if !witness_arrival_installed(&tx)? {
            tx.execute_batch(ADD_WITNESS_ARRIVAL)?;
            // Earlier prepared gaps may have been recorded without a missing page ID.
            tx.execute_batch(
                "INSERT INTO rep_search_waiting(source,head,object,ready)
                 SELECT g.object,g.head,g.object,1 FROM rep_search_gaps g
                 JOIN archive_exchange_sessions s ON s.head=g.head
                 WHERE s.verified AND s.complete
                 ON CONFLICT(source,head,object) DO UPDATE SET ready=1;
                 UPDATE rep_search_revision SET value=value+1;",
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(super) fn refresh_search(
        &self,
        inventory: &mut InventoryReader<'_>,
        head: &str,
        mut budget: usize,
    ) -> Result<()> {
        self.initialize_visibility(inventory.id()?, head)?;
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
                inventory.records_after(state.cursor, count + 1)?
            } else {
                ready
            };
            let finished = records.len() <= count;
            let indexed: Vec<_> = records
                .into_iter()
                .take(count)
                .map(|id| {
                    let record = cached_record(&self.db, inventory, id)?
                        .unwrap_or_else(|| Record::Extracted(self.search_sections(inventory, id)));
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
        self.refresh_text(inventory, head, budget.min(TEXT_BATCH))
    }

    fn refresh_text(
        &self,
        inventory: &mut InventoryReader<'_>,
        head: &str,
        budget: usize,
    ) -> Result<()> {
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
                    "SELECT t.entry,e.object,t.cursor,t.length FROM rep_search_visible v
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
            let indexed = index_text_window(
                inventory,
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
                   AND EXISTS(SELECT 1 FROM rep_search_visible WHERE head=?4 AND entry=?1)",
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
                         SELECT head,object FROM rep_search_visible WHERE entry=?",
                        [entry],
                    )?;
                }
            }
            tx.commit()?;
        }
        Ok(())
    }
}

pub(super) fn current_schema(db: &Connection) -> Result<bool> {
    if installed_version(db)? != Some(INDEX_VERSION) {
        return Ok(false);
    }
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema
            WHERE type='index' AND name='rep_search_media_passages')",
        [],
        |row| row.get(0),
    )?)
}

fn witness_arrival_installed(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE
            type='trigger' AND name='rep_search_witness_arrival')",
        [],
        |row| row.get(0),
    )?)
}

fn vector_progress_installed(db: &Connection) -> Result<bool> {
    Ok(db
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE
            type='table' AND name='rep_search_vector_progress')",
        )?
        .query_row([], |row| row.get(0))?)
}

pub(super) fn prepared_cache_version(db: &Connection) -> Result<Option<u32>> {
    match installed_version(db)? {
        version @ Some(6 | INDEX_VERSION) => Ok(version),
        None | Some(1..=5) => Ok(None),
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
                    JOIN rep_search_visible v ON v.entry=t.entry WHERE v.head=?1 AND t.cursor<t.length),
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

fn cached_record(
    db: &Connection,
    inventory: &mut InventoryReader<'_>,
    id: Id,
) -> Result<Option<Record>> {
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
    let mut checked = BTreeSet::new();
    while let Some(row) = rows.next()? {
        let id = row.get::<_, String>(1)?.parse()?;
        if checked.insert(id) && inventory.readable(id).is_err() {
            return Ok(None);
        }
        let entry: i64 = row.get(0)?;
        if entries.last() != Some(&entry) {
            entries.push(entry);
        }
    }
    Ok(Some(Record::Cached { entries, limited }))
}

pub(super) fn entry_readable(
    db: &Connection,
    inventory: &mut InventoryReader<'_>,
    entry: i64,
) -> Result<bool> {
    let mut statement = db.prepare_cached("SELECT source FROM rep_search_sources WHERE entry=?")?;
    let mut rows = statement.query([entry])?;
    let mut found = false;
    while let Some(row) = rows.next()? {
        found = true;
        if inventory
            .readable(row.get::<_, String>(0)?.parse()?)
            .is_err()
        {
            return Ok(false);
        }
    }
    Ok(found)
}

fn publish(db: &Connection, head: &str, id: Id, record: &Record) -> Result<()> {
    let key = id.to_string();
    db.execute(
        "INSERT OR IGNORE INTO rep_search_sets(writer) VALUES(?)",
        [head],
    )?;
    db.execute("INSERT OR IGNORE INTO rep_search_head_sets SELECT ?1,id FROM rep_search_sets WHERE writer=?1",[head])?;
    db.execute(
        "DELETE FROM rep_search_set_entries WHERE set_id=(SELECT id FROM rep_search_sets WHERE writer=?1)
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
            for page in &extracted.missing_pages {
                db.execute(
                    "INSERT OR IGNORE INTO rep_search_waiting(source,head,object,ready)
                     VALUES('page/' || ?1,?2,?3,EXISTS(SELECT 1 FROM custody_pages WHERE id=?1))",
                    params![page.to_string(), head, key],
                )?;
            }
            (entries, extracted.limited)
        }
    };
    for entry in entries {
        db.execute(
            "INSERT OR IGNORE INTO rep_search_set_entries(set_id,entry,object,section)
             SELECT s.id,e.id,e.object,e.section FROM rep_search_sets s,rep_search_entries e WHERE s.writer=?1 AND e.id=?2
             AND NOT EXISTS(SELECT 1 FROM rep_search_visible WHERE head=?1 AND entry=?2)",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        profile::{Kind, PUBLIC},
        wire::Format,
    };
    use serde_json::json;

    #[test]
    fn lexical_preparation_seeks_across_blob_pages_between_record_formats() -> Result<()> {
        let archive = Archive::memory()?;
        let public = Id::blob(PUBLIC);
        let record = Document::new(
            Kind::Question,
            "samples",
            "notebook",
            json!({"statement":"Which leaf belongs in the sample?"}),
            vec![],
        )?;
        let first = archive.document(&record, Format::A, public)?;
        let last = archive.document(&record, Format::B, public)?;
        for sample in 0..2048u16 {
            archive.blob(&sample.to_be_bytes(), public)?;
        }
        let head = archive.snapshot_id("notebook", Default::default())?;
        let full_page_count = archive.head(head)?.pages.len() as u64;
        let key = head.to_string();

        for (record, complete) in [(first, false), (last, true)] {
            let mut inventory = archive.inventory_reader(head)?;
            archive.prepare_lexical_index(&mut inventory, &key, 1, false)?;
            let state = state(&archive.db, &key)?.expect("search was prepared");
            assert_eq!(state.cursor, Some(record));
            assert_eq!(coverage(&archive.db, &key)?, (complete, false));
            assert!(
                inventory.tree.io.backing_reads < full_page_count / 2,
                "a one-record step must not load the blob range"
            );
        }
        Ok(())
    }
}
