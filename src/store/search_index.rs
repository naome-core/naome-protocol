mod cache;
mod extract;
mod rank;
mod text;
mod vectors;

use self::rank::{SemanticQuery, VectorWork, rank};
use super::{Archive, Snapshot};
use crate::{
    profile::Document,
    search::{
        Embedder, MAX_MEDIA_BYTES, SearchHit, SearchInput, SearchLocation, SearchMode,
        SearchOptions, SearchPreparation, SearchReport, native_media,
    },
    wire::Id,
};
use anyhow::{Context, Result, ensure};
use cache::{INDEX_VERSION, coverage, prepared_cache_available};
use rusqlite::params;
use std::collections::BTreeSet;
use vectors::{embed_query, missing_vectors};

const MAX_QUERY_PASSAGES: usize = 24;

pub(crate) enum IndexPreparation {
    Existing,
    Lexical,
    Full { rebuild: bool },
}

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
    cacheable: bool,
}

struct IndexedSection {
    sources: BTreeSet<Id>,
    passages: Vec<Passage>,
    pending_text: Option<u64>,
}

impl Archive {
    /// Resolve an explicit head, the pinned head, or the archive's stable default for search.
    pub fn search_head(&self, head: Option<Id>) -> Result<Snapshot> {
        let head = match head {
            Some(head) => head,
            None => self
                .db
                .query_row(
                    "SELECT coalesce(
                        (SELECT head FROM rep_search_selection WHERE singleton=1),
                        (SELECT id FROM heads
                         WHERE NOT EXISTS (SELECT 1 FROM rep_imported_heads WHERE head=heads.id)
                         ORDER BY rowid DESC LIMIT 1),
                        (SELECT id FROM heads ORDER BY rowid LIMIT 1)
                    )",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )?
                .context("no archive snapshot; create one with snapshot or specify --head")?
                .parse()?,
        };
        self.head(head)
    }

    /// Pin a search snapshot independently of cache preparation. None restores the default.
    pub fn set_search_head(&self, head: Option<Id>) -> Result<()> {
        if let Some(head) = head {
            self.head(head)?;
            self.db.execute(
                "INSERT OR REPLACE INTO rep_search_selection VALUES(1,?)",
                [head.to_string()],
            )?;
        } else {
            self.db.execute("DELETE FROM rep_search_selection", [])?;
        }
        Ok(())
    }

    /// Clear one snapshot's derived search data. The next search rebuilds it.
    pub fn clear_search_index(&self, head: Id) -> Result<()> {
        self.init_search()?;
        self.clear_index_view(&head.to_string())
    }

    fn clear_index_view(&self, head: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM rep_search_heads WHERE head=?", [head])?;
        Ok(())
    }

    fn prepare_lexical_index(
        &self,
        snapshot: &Snapshot,
        head: &str,
        budget: usize,
        rebuild: bool,
    ) -> Result<()> {
        self.init_search()?;
        if rebuild {
            self.clear_index_view(head)?;
        }
        self.refresh_search(snapshot, head, budget)
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
        let preparation = if options.mode == SearchMode::Lexical {
            IndexPreparation::Lexical
        } else {
            IndexPreparation::Full { rebuild: false }
        };
        self.lookup_search(snapshot, input, options, model, preparation)
    }

    /// Query the prepared lexical and embedding cache.
    pub fn search_prepared(
        &self,
        snapshot: &Snapshot,
        input: SearchInput<'_>,
        options: SearchOptions<'_>,
        model: Option<&mut dyn Embedder>,
    ) -> Result<SearchReport> {
        self.lookup_search(snapshot, input, options, model, IndexPreparation::Existing)
    }

    /// Prepare a bounded slice independently of any query.
    pub fn prepare_search(
        &self,
        snapshot: &Snapshot,
        budget: usize,
        model: Option<&mut dyn Embedder>,
    ) -> Result<SearchPreparation> {
        self.prepare_search_index(snapshot, budget, model, false)
    }

    pub(crate) fn prepare_search_index(
        &self,
        snapshot: &Snapshot,
        budget: usize,
        model: Option<&mut dyn Embedder>,
        rebuild: bool,
    ) -> Result<SearchPreparation> {
        ensure!(budget > 0, "positive index budget required");
        let head = snapshot.id()?;
        let key = head.to_string();
        self.prepare_lexical_index(snapshot, &key, budget, rebuild)?;
        let model_key = if let Some(model) = model {
            let id = model.id().to_string();
            self.refresh_search_vectors(snapshot, &key, model, budget)?;
            Some(id)
        } else {
            None
        };
        let read = self.db.unchecked_transaction()?;
        let (index_complete, limited_coverage) = coverage(&read, &key)?;
        let (semantic_index_complete, limited_coverage) = semantic_coverage(
            &read,
            &key,
            model_key.as_deref(),
            index_complete,
            limited_coverage,
        )?;
        read.commit()?;
        Ok(SearchPreparation {
            head,
            index_complete,
            semantic_index_complete,
            limited_coverage,
        })
    }

    pub(crate) fn lookup_search(
        &self,
        snapshot: &Snapshot,
        input: SearchInput<'_>,
        options: SearchOptions<'_>,
        model: Option<&mut dyn Embedder>,
        preparation: IndexPreparation,
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
            vector_products: 0,
            vector_rows_examined: 0,
            vector_rows_loaded: 0,
            limited_coverage: false,
            query_limited: limited,
            hits: Vec::new(),
        };

        if let Some(id) = input.exact_id() {
            let read = self.db.unchecked_transaction()?;
            if let Ok(bytes) = self.resolve(snapshot, id, "read") {
                let document = id.format().and_then(|format| {
                    Self::decode_document(format, &bytes, |profile| {
                        self.resolve(snapshot, profile, "read")
                    })
                    .ok()
                });
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
        let mut model = if mode == SearchMode::Lexical {
            None
        } else {
            Some(model.context("semantic search requires a configured local embedding model")?)
        };
        let head = head.to_string();
        if matches!(preparation, IndexPreparation::Existing) && !prepared_cache_available(&self.db)?
        {
            report.semantic_index_complete = model.as_ref().map(|_| false);
            return Ok(report);
        }
        let query_vectors = if let Some(model) = model.as_deref_mut() {
            report.model = Some(model.id());
            let minimum = model.minimum_similarity();
            ensure!(
                minimum.is_finite() && (-1.0..=1.0).contains(&minimum),
                "invalid minimum cosine similarity"
            );
            report.minimum_similarity = Some(minimum);
            let (query_vectors, limited) = embed_query(model, &text, media)?;
            report.query_limited |= limited;
            Some(query_vectors)
        } else {
            None
        };
        match preparation {
            IndexPreparation::Existing => {}
            IndexPreparation::Lexical => {
                self.prepare_lexical_index(snapshot, &head, options.index_budget, false)?;
            }
            IndexPreparation::Full { rebuild } => {
                self.prepare_lexical_index(snapshot, &head, options.index_budget, rebuild)?;
                if let Some(model) = model {
                    self.refresh_search_vectors(snapshot, &head, model, options.index_budget)?;
                }
            }
        }
        let read = self.db.unchecked_transaction()?;
        (report.index_complete, report.limited_coverage) = coverage(&read, &head)?;
        let model_key = report.model.map(|id| id.to_string());
        (report.semantic_index_complete, report.limited_coverage) = semantic_coverage(
            &read,
            &head,
            model_key.as_deref(),
            report.index_complete,
            report.limited_coverage,
        )?;
        if mode == SearchMode::Lexical {
            let media_exists: bool = read.query_row(
                "SELECT EXISTS(SELECT 1 FROM rep_search_passages p JOIN rep_search_visibility v ON v.entry=p.entry
                    WHERE v.head=? AND p.semantic AND p.text='')",
                [&head],
                |row| row.get(0),
            )?;
            report.limited_coverage |= media_exists;
        }
        let mut work = VectorWork::default();
        let candidates = rank(
            &read,
            &head,
            &text,
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
            &mut work,
        )?;
        report.vector_budget_exhausted = work.bounded;
        report.vector_products = work.products;
        report.vector_rows_examined = work.rows_examined;
        report.vector_rows_loaded = work.vectors_loaded;
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
                // Candidate visibility and the withdrawal trigger cover every
                // source dependency. Recovery verifies the original payload bytes.
                let part = candidate
                    .semantic
                    .and_then(|item| item.part)
                    .context("missing ranked media part")?;
                let encoded: String = read.query_row(
                    "SELECT span FROM rep_search_vectors WHERE model=? AND passage=? AND part=?",
                    params![model_key, candidate.passage(), part],
                    |row| row.get(0),
                )?;
                *span = serde_json::from_str(&encoded)?;
                None
            } else {
                let Some(snippet) = self
                    .search_snippet(snapshot, &document, &location)
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
}

fn semantic_coverage(
    db: &rusqlite::Connection,
    head: &str,
    model: Option<&str>,
    complete: bool,
    limited: bool,
) -> Result<(Option<bool>, bool)> {
    let Some(model) = model else {
        return Ok((None, limited));
    };
    let limited_vectors: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM rep_search_vector_state st
            JOIN rep_search_passages p ON p.id=st.passage
            JOIN rep_search_visibility v ON v.entry=p.entry
            WHERE v.head=? AND st.model=? AND st.limited)",
        params![head, model],
        |row| row.get(0),
    )?;
    Ok((
        Some(complete && !missing_vectors(db, head, model)?),
        limited || limited_vectors,
    ))
}
