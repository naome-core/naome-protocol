mod extract;
mod rank;
mod vectors;

use self::{
    extract::{normalize, terms},
    rank::{SemanticQuery, rank},
};
use super::{Archive, Snapshot};
use crate::{
    profile::Document,
    search::{
        Embedder, MAX_MEDIA_BYTES, SearchHit, SearchInput, SearchLocation, SearchMode,
        SearchOptions, SearchReport, native_media,
    },
    wire::Id,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::{
    collections::BTreeSet,
    ops::Bound::{Excluded, Unbounded},
};
use vectors::{embed_query, missing_vectors};

const INDEX_VERSION: u32 = 1;
const RECORD_BATCH: usize = 16;
const MAX_QUERY_PASSAGES: usize = 24;

struct Passage {
    text: String,
    location: SearchLocation,
    priority: i64,
    semantic: bool,
}

#[derive(Default)]
struct Dependencies {
    available: BTreeSet<Id>,
    missing: BTreeSet<Id>,
}

struct IndexedRecord {
    document: Option<Document>,
    sections: Vec<IndexedSection>,
    missing: BTreeSet<Id>,
    limited: bool,
}

struct IndexedSection {
    sources: BTreeSet<Id>,
    passages: Vec<Passage>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct State {
    cursor: Option<Id>,
    dirty: bool,
    finished: bool,
    limited: bool,
}

impl Archive {
    /// Resolve an explicit head, or use the most recently saved or imported inventory.
    pub fn search_head(&self, head: Option<Id>) -> Result<Snapshot> {
        let head = match head {
            Some(head) => head,
            None => self
                .db
                .query_row(
                    "SELECT id FROM heads ORDER BY rowid DESC LIMIT 1",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .context("no archive snapshot; create one with snapshot or specify --head")?
                .parse()?,
        };
        self.head(head)
    }

    /// Clear one snapshot's derived search data. The next search rebuilds it.
    pub fn clear_search_index(&self, head: Id) -> Result<()> {
        self.init_search()?;
        self.db.execute(
            "DELETE FROM rep_search_heads WHERE head=?",
            [head.to_string()],
        )?;
        Ok(())
    }

    pub fn find(
        &self,
        snapshot: &Snapshot,
        query: &str,
        options: SearchOptions<'_>,
        model: Option<&mut dyn Embedder>,
    ) -> Result<SearchReport> {
        self.find_input(snapshot, SearchInput::Text(query), options, model)
    }

    pub fn find_input(
        &self,
        snapshot: &Snapshot,
        input: SearchInput<'_>,
        options: SearchOptions<'_>,
        model: Option<&mut dyn Embedder>,
    ) -> Result<SearchReport> {
        let (text, media, limited) = match input {
            SearchInput::Text(query) => {
                let query = query.trim();
                ensure!(
                    !query.is_empty() && query.len() <= 4096,
                    "expected a search query of 1 to 4096 bytes"
                );
                (vec![query], None, false)
            }
            SearchInput::TextFile(text) => {
                let mut passages: Vec<_> = extract::ranges(text)
                    .take(MAX_QUERY_PASSAGES + 1)
                    .map(|range| &text[range])
                    .collect();
                ensure!(!passages.is_empty(), "empty query document");
                let limited = passages.len() > MAX_QUERY_PASSAGES;
                passages.truncate(MAX_QUERY_PASSAGES);
                (passages, None, limited)
            }
            SearchInput::Media(media) => {
                ensure!(
                    native_media(media.media_type),
                    "unsupported search input media type"
                );
                ensure!(
                    !media.bytes.is_empty() && media.bytes.len() as u64 <= MAX_MEDIA_BYTES,
                    "search media input exceeds its byte budget or is empty"
                );
                ensure!(
                    options.mode != SearchMode::Lexical,
                    "media queries require semantic or hybrid search"
                );
                (Vec::new(), Some(media), false)
            }
        };
        let query = text.first().copied().unwrap_or_default();
        ensure!(
            (1..=100).contains(&options.limit),
            "search limit must be between 1 and 100"
        );
        ensure!(
            options.index_budget > 0 && options.vector_budget > 0,
            "positive search budgets required"
        );
        let head = snapshot.id()?;
        let mut report = SearchReport {
            head,
            mode: options.mode,
            index_version: INDEX_VERSION,
            model: None,
            minimum_similarity: None,
            index_complete: false,
            semantic_index_complete: None,
            vector_budget_exhausted: false,
            limited_coverage: false,
            query_limited: limited || (options.mode == SearchMode::Lexical && text.len() > 1),
            hits: Vec::new(),
        };

        if matches!(input, SearchInput::Text(_))
            && let Ok(id) = query.parse::<Id>()
        {
            let read = self.db.unchecked_transaction()?;
            if self.resolve(snapshot, id, "read").is_ok() {
                let document = self.read(snapshot, id, "read").ok();
                if options
                    .kind
                    .is_none_or(|kind| document.as_ref().is_some_and(|d| d.kind == kind))
                    && options.subject.is_none_or(|subject| {
                        document.as_ref().is_some_and(|d| d.subject == subject)
                    })
                    && options
                        .scope
                        .is_none_or(|scope| document.as_ref().is_some_and(|d| d.scope == scope))
                {
                    report.hits.push(self.search_hit(
                        snapshot,
                        id,
                        document.as_ref(),
                        SearchLocation::Object,
                        None,
                        true,
                    ));
                }
            }
            read.commit()?;
            report.index_complete = true;
            return Ok(report);
        }

        let mode = options.mode;
        let model = if mode == SearchMode::Lexical {
            None
        } else {
            Some(model.context("semantic search requires a configured local embedding model")?)
        };
        let head = head.to_string();
        self.init_search()?;
        self.refresh_search(snapshot, &head, options.index_budget)?;
        let query_vectors = if let Some(model) = model {
            report.model = Some(model.id());
            let minimum = model.minimum_similarity();
            ensure!(
                minimum.is_finite() && (-1.0..=1.0).contains(&minimum),
                "invalid minimum cosine similarity"
            );
            report.minimum_similarity = Some(minimum);
            let (query_vectors, limited) = embed_query(model, &text, media)?;
            report.query_limited |= limited;
            self.refresh_search_vectors(snapshot, &head, model, options.index_budget)?;
            Some(query_vectors)
        } else {
            None
        };
        let read = self.db.unchecked_transaction()?;
        let state = read_state(&read, &head)?.context("search index disappeared")?;
        report.index_complete = state.finished && !state.dirty;
        report.limited_coverage = state.limited;
        let model_key = report.model.map(|id| id.to_string());
        if let Some(key) = model_key.as_deref() {
            report.semantic_index_complete =
                Some(report.index_complete && !missing_vectors(&read, &head, key)?);
            let limited_media: bool = read.query_row(
                "SELECT EXISTS(SELECT 1 FROM rep_search_limited_media WHERE head=? AND model=?)",
                params![head, key],
                |row| row.get(0),
            )?;
            report.limited_coverage |= limited_media;
        }
        if mode == SearchMode::Lexical {
            let media_exists: bool = read.query_row(
                "SELECT EXISTS(SELECT 1 FROM rep_search_passages WHERE head=? AND semantic AND text='')",
                [&head],
                |row| row.get(0),
            )?;
            report.limited_coverage |= media_exists;
        }
        let candidates = rank(
            &read,
            &head,
            query,
            options,
            model_key
                .as_deref()
                .zip(query_vectors.as_deref())
                .zip(report.minimum_similarity)
                .map(|((model, vector), minimum)| SemanticQuery {
                    model,
                    vectors: vector,
                    minimum,
                }),
            &mut report.vector_budget_exhausted,
        )?;
        for candidate in candidates {
            let Ok(document) = self.read(snapshot, candidate.id, "read") else {
                report.limited_coverage = true;
                continue;
            };
            let location: String = read.query_row(
                "SELECT location FROM rep_search_passages WHERE id=?",
                [candidate.passage()],
                |row| row.get(0),
            )?;
            let mut location = serde_json::from_str(&location)?;
            let snippet = if let SearchLocation::Media { span, .. } = &mut location {
                // The cache stores selectors and embeddings. Originals must still
                // resolve through the live archive before becoming search hits.
                if self
                    .materialize(
                        snapshot,
                        candidate.id,
                        "read",
                        MAX_MEDIA_BYTES,
                        std::io::sink(),
                    )
                    .is_err()
                {
                    report.limited_coverage = true;
                    continue;
                }
                let part = candidate
                    .semantic
                    .and_then(|item| item.part)
                    .context("missing ranked media part")?;
                let encoded: String = read.query_row(
                    "SELECT span FROM rep_search_vectors WHERE head=? AND model=? AND passage=? AND part=?",
                    params![head, model_key, candidate.passage(), part],
                    |row| row.get(0),
                )?;
                *span = serde_json::from_str(&encoded)?;
                None
            } else {
                let Some(snippet) = self
                    .search_snippet(snapshot, candidate.id, &document, &location)
                    .ok()
                    .flatten()
                else {
                    report.limited_coverage = true;
                    continue;
                };
                Some(snippet)
            };
            let mut hit = self.search_hit(
                snapshot,
                candidate.id,
                Some(&document),
                location,
                snippet,
                false,
            );
            hit.lexical_rank = candidate.lexical.map(|item| item.rank);
            hit.semantic_rank = candidate.semantic.map(|item| item.rank);
            hit.semantic_similarity = candidate.semantic.and_then(|item| item.similarity);
            report.hits.push(hit);
        }
        read.commit()?;
        Ok(report)
    }

    fn search_hit(
        &self,
        snapshot: &Snapshot,
        id: Id,
        document: Option<&Document>,
        location: SearchLocation,
        snippet: Option<String>,
        exact_id: bool,
    ) -> SearchHit {
        SearchHit {
            id,
            kind: document.map(|d| d.kind),
            subject: document.map(|d| d.subject.clone()),
            scope: document.map(|d| d.scope.clone()),
            media_type: document
                .filter(|d| {
                    d.kind == crate::profile::Kind::Descriptor && d.scope == "ordered-chunks-1"
                })
                .map(|d| d.subject.clone()),
            location,
            snippet,
            exact_id,
            lexical_rank: None,
            semantic_rank: None,
            semantic_similarity: None,
            links: document
                .into_iter()
                .flat_map(|d| &d.inputs)
                .filter(|link| link.role != "chunk")
                .take(8)
                .filter(|link| self.resolve(snapshot, link.target, "read").is_ok())
                .cloned()
                .collect(),
        }
    }

    fn init_search(&self) -> Result<()> {
        let exists = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='rep_search_schema')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
            tx.execute_batch(include_str!("search_index/schema.sql"))?;
            tx.commit()?;
        }
        let version: u32 =
            self.db
                .query_row("SELECT version FROM rep_search_schema", [], |row| {
                    row.get(0)
                })?;
        ensure!(
            version == INDEX_VERSION,
            "unsupported local search index version"
        );
        Ok(())
    }

    fn search_state(&self, head: &str) -> Result<State> {
        let state = read_state(&self.db, head)?;
        if let Some(state) = state
            && !state.dirty
        {
            return Ok(state);
        }
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM rep_search_heads WHERE head=? AND dirty",
            [head],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO rep_search_heads(head) VALUES(?)",
            [head],
        )?;
        let state = read_state(&tx, head)?.context("search index disappeared")?;
        tx.commit()?;
        Ok(state)
    }

    fn refresh_search(&self, snapshot: &Snapshot, head: &str, mut budget: usize) -> Result<()> {
        let mut state = self.search_state(head)?;
        while !state.finished && budget != 0 {
            let revision = revision(&self.db)?;
            let bound = state.cursor.map_or(Unbounded, Excluded);
            let count = budget.min(RECORD_BATCH);
            let records: Vec<_> = snapshot
                .objects
                .range((bound, Unbounded))
                .copied()
                .filter(|id| id.format().is_some())
                .take(count + 1)
                .collect();
            let finished = records.len() <= count;
            let indexed: Vec<_> = records
                .into_iter()
                .take(count)
                .map(|id| (id, self.search_sections(snapshot, id)))
                .collect();

            let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
            if revision != self::revision(&tx)? || read_state(&tx, head)? != Some(state) {
                return Ok(());
            }
            for (id, record) in &indexed {
                if let Some(document) = &record.document {
                    write_sections(&tx, head, *id, document, &record.sections)?;
                }
                for missing in &record.missing {
                    tx.execute(
                        "INSERT OR IGNORE INTO rep_search_waiting VALUES(?,?)",
                        params![missing.to_string(), head],
                    )?;
                }
                state.cursor = Some(*id);
                state.limited |= record.limited;
            }
            state.finished = finished;
            tx.execute(
                "UPDATE rep_search_heads SET cursor=?,finished=?,limited=? WHERE head=?",
                params![
                    state.cursor.map(|id| id.to_string()),
                    state.finished,
                    state.limited,
                    head
                ],
            )?;
            tx.commit()?;
            budget -= indexed.len();
        }
        Ok(())
    }
}

fn revision(db: &Connection) -> Result<i64> {
    Ok(
        db.query_row("SELECT value FROM rep_search_revision", [], |row| {
            row.get(0)
        })?,
    )
}

fn read_state(db: &Connection, head: &str) -> Result<Option<State>> {
    let fields = db
        .query_row(
            "SELECT cursor,dirty,finished,limited FROM rep_search_heads WHERE head=?",
            [head],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                ))
            },
        )
        .optional()?;
    fields
        .map(|(cursor, dirty, finished, limited)| {
            Ok(State {
                cursor: cursor.map(|id| id.parse()).transpose()?,
                dirty,
                finished,
                limited,
            })
        })
        .transpose()
}

fn write_sections(
    db: &Connection,
    head: &str,
    id: Id,
    document: &Document,
    sections: &[IndexedSection],
) -> Result<()> {
    let mut entry = db.prepare_cached(
        "INSERT INTO rep_search_entries(head,object,kind,subject,scope) VALUES(?,?,?,?,?)",
    )?;
    let mut source = db.prepare_cached("INSERT INTO rep_search_sources VALUES(?,?)")?;
    let mut row = db.prepare_cached(
        "INSERT INTO rep_search_passages(head,entry,location,text,normalized,priority,semantic) VALUES(?,?,?,?,?,?,?)",
    )?;
    let mut posting = db.prepare_cached("INSERT INTO rep_search_terms VALUES(?,?,?)")?;
    for section in sections
        .iter()
        .filter(|section| !section.passages.is_empty())
    {
        let entry = entry.insert(params![
            head,
            id.to_string(),
            document.kind as i64,
            document.subject,
            document.scope
        ])?;
        for id in &section.sources {
            source.execute(params![id.to_string(), entry])?;
        }
        for passage in &section.passages {
            let normalized = normalize(&passage.text);
            let location = serde_json::to_string(&passage.location)?;
            let id = row.insert(params![
                head,
                entry,
                location,
                passage.text,
                normalized,
                passage.priority,
                passage.semantic
            ])?;
            for term in terms(&normalized) {
                posting.execute(params![head, term, id])?;
            }
        }
    }
    Ok(())
}
