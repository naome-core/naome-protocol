use crate::{
    search::{
        EmbeddedMedia, Embedder, EmbeddingInput, MAX_MEDIA_BYTES, MediaInput, MediaSpan,
        SearchLocation,
    },
    store::{Archive, Snapshot},
    wire::Id,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use std::time::{SystemTime, UNIX_EPOCH};

const VECTOR_BATCH: usize = 16;
const MAX_MEDIA_PARTS: usize = 48;
const RETRY_SECONDS: i64 = 30;

struct Pending {
    id: i64,
    object: Id,
    location: SearchLocation,
    text: String,
}

#[derive(Default)]
struct CachedVectors {
    parts: Vec<(Option<MediaSpan>, Vec<f32>)>,
    limited: bool,
    retry: bool,
}

pub(super) fn embed_query(
    model: &mut dyn Embedder,
    text: &[&str],
    media: Option<MediaInput<'_>>,
) -> Result<(Vec<Vec<f32>>, bool)> {
    if let Some(media) = media {
        let embedded = media_embeddings(model, EmbeddingInput::Query, media)?;
        ensure!(
            !embedded.parts.is_empty(),
            "media query could not be decoded"
        );
        let vectors = embedded.parts.into_iter().map(|part| part.vector).collect();
        Ok((vectors, embedded.limited))
    } else {
        let mut vectors = Vec::new();
        for batch in text.chunks(VECTOR_BATCH) {
            vectors.extend(text_embeddings(model, EmbeddingInput::Query, batch)?);
        }
        Ok((vectors, false))
    }
}

impl Archive {
    pub(super) fn refresh_search_vectors(
        &self,
        snapshot: &Snapshot,
        head: &str,
        model: &mut dyn Embedder,
        mut budget: usize,
    ) -> Result<()> {
        let position_budget = budget;
        let key = model.id().to_string();
        let mut cursor = 0;
        let now = unix_seconds()?;
        while budget > 0 {
            let pending =
                self.pending_vectors(head, &key, cursor, now, budget.min(VECTOR_BATCH))?;
            if pending.is_empty() {
                break;
            }
            let indexed = self.embed_passages(snapshot, &pending, model);
            let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
            {
                let mut state = tx.prepare_cached(
                    "INSERT INTO rep_search_vector_state(passage,model,limited,retry_after)
                     SELECT id,?2,?3,?4 FROM rep_search_passages WHERE id=?1
                     ON CONFLICT(passage,model) DO UPDATE SET limited=excluded.limited,retry_after=excluded.retry_after
                     WHERE rep_search_vector_state.retry_after IS NOT NULL",
                )?;
                let mut insert = tx.prepare_cached(
                    "INSERT OR IGNORE INTO rep_search_vectors(passage,model,part,span,vector)
                     VALUES(?1,?2,?3,?4,?5)",
                )?;
                for (passage, indexed) in pending.iter().zip(indexed) {
                    let retry = indexed.retry.then_some(now.saturating_add(RETRY_SECONDS));
                    if state.execute(params![passage.id, key, indexed.limited, retry])? == 0 {
                        continue;
                    }
                    for (part, (span, vector)) in indexed.parts.into_iter().enumerate() {
                        let bytes: Vec<_> = vector.into_iter().flat_map(f32::to_le_bytes).collect();
                        let span = span.map(|span| serde_json::to_string(&span)).transpose()?;
                        insert.execute(params![passage.id, key, part as i64, span, bytes])?;
                    }
                }
            }
            // A removed passage cannot receive an embedding. The database assigns
            // monotonically increasing passage IDs, including after a rebuild.
            cursor = pending.last().context("empty embedding batch")?.id;
            tx.commit()?;
            budget -= pending.len();
        }
        if !super::positions::ready(&self.db, head, &key)?
            && super::coverage(&self.db, head)?.0
            && !missing_vectors(&self.db, head, &key)?
        {
            self.prepare_positions(head, &key, position_budget)?;
        }
        Ok(())
    }

    fn pending_vectors(
        &self,
        head: &str,
        model: &str,
        cursor: i64,
        now: i64,
        limit: usize,
    ) -> Result<Vec<Pending>> {
        self.db
            .prepare_cached(
                "SELECT p.id,e.object,p.location,p.text FROM rep_search_visible v
             JOIN rep_search_entries e ON e.id=v.entry
             JOIN rep_search_passages p ON p.entry=e.id
             LEFT JOIN rep_search_vector_state st ON st.passage=p.id AND st.model=?2
             WHERE v.head=?1 AND p.semantic AND p.id>?3
               AND (st.passage IS NULL OR st.retry_after<=?4) ORDER BY p.id LIMIT ?5",
            )?
            .query_map(params![head, model, cursor, now, limit as i64], |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get(3)?,
                ))
            })?
            .map(|row| {
                let (id, object, location, text) = row?;
                Ok(Pending {
                    id,
                    object: object.parse()?,
                    location: serde_json::from_str(&location)?,
                    text,
                })
            })
            .collect()
    }

    fn embed_passages(
        &self,
        snapshot: &Snapshot,
        pending: &[Pending],
        model: &mut dyn Embedder,
    ) -> Vec<CachedVectors> {
        let text: Vec<_> = pending
            .iter()
            .filter(|passage| !matches!(passage.location, SearchLocation::Media { .. }))
            .map(|passage| passage.text.as_str())
            .collect();
        let mut texts = if text.is_empty() {
            Vec::new()
        } else {
            text_embeddings(model, EmbeddingInput::Passage, &text).unwrap_or_default()
        }
        .into_iter();
        pending
            .iter()
            .map(|pending| {
                let SearchLocation::Media { media_type, .. } = &pending.location else {
                    return texts.next().map_or_else(CachedVectors::retry, |embedding| {
                        CachedVectors {
                            parts: vec![(None, embedding)],
                            limited: false,
                            retry: false,
                        }
                    });
                };
                let mut bytes = Vec::new();
                if self
                    .materialize(
                        snapshot,
                        pending.object,
                        "read",
                        MAX_MEDIA_BYTES,
                        &mut bytes,
                    )
                    .is_err()
                {
                    return CachedVectors {
                        limited: true,
                        ..Default::default()
                    };
                }
                match media_embeddings(
                    model,
                    EmbeddingInput::Passage,
                    MediaInput {
                        media_type,
                        bytes: &bytes,
                    },
                ) {
                    Ok(embedded) => CachedVectors {
                        parts: embedded
                            .parts
                            .into_iter()
                            .map(|part| (Some(part.span), part.vector))
                            .collect(),
                        limited: embedded.limited,
                        retry: false,
                    },
                    Err(_) => CachedVectors::retry(),
                }
            })
            .collect()
    }
}

impl CachedVectors {
    fn retry() -> Self {
        Self {
            limited: true,
            retry: true,
            ..Default::default()
        }
    }
}

fn unix_seconds() -> Result<i64> {
    Ok(i64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    )?)
}

pub(super) fn missing_vectors(db: &Connection, head: &str, model: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM rep_search_visible v
         JOIN rep_search_passages p ON p.entry=v.entry
         LEFT JOIN rep_search_vector_state st ON st.passage=p.id AND st.model=?2
         WHERE v.head=?1 AND p.semantic AND (st.passage IS NULL OR st.retry_after IS NOT NULL))",
        params![head, model],
        |row| row.get(0),
    )?)
}

fn text_embeddings(
    model: &mut dyn Embedder,
    input: EmbeddingInput,
    texts: &[&str],
) -> Result<Vec<Vec<f32>>> {
    let mut vectors = model.embed(input, texts)?;
    ensure!(vectors.len() == texts.len(), "wrong number of embeddings");
    for vector in &mut vectors {
        normalize_vector(vector, model.dimensions())?;
    }
    Ok(vectors)
}

fn media_embeddings(
    model: &mut dyn Embedder,
    input: EmbeddingInput,
    media: MediaInput<'_>,
) -> Result<EmbeddedMedia> {
    let mut result = model.embed_media(input, media)?;
    ensure!(
        result.parts.len() <= MAX_MEDIA_PARTS,
        "invalid number of media embeddings"
    );
    result.limited |= result.parts.is_empty();
    for part in &mut result.parts {
        ensure!(
            match part.span {
                MediaSpan::Whole => true,
                MediaSpan::Page { number } => number > 0,
                MediaSpan::Time { start_ms, end_ms } => start_ms < end_ms,
            },
            "invalid embedding selector"
        );
        normalize_vector(&mut part.vector, model.dimensions())?;
    }
    Ok(result)
}

fn normalize_vector(vector: &mut [f32], dimensions: usize) -> Result<()> {
    ensure!(
        (1..=4096).contains(&dimensions) && vector.len() == dimensions,
        "wrong embedding dimensions"
    );
    let sum: f64 = vector.iter().map(|value| f64::from(*value).powi(2)).sum();
    ensure!(sum.is_finite() && sum > 0.0, "invalid embedding");
    let norm = sum.sqrt();
    for value in vector {
        *value = (f64::from(*value) / norm) as f32;
    }
    Ok(())
}
