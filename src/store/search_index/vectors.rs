use crate::{
    search::{
        EmbeddedMedia, Embedder, EmbeddingInput, MAX_MEDIA_BYTES, MediaInput, MediaSpan,
        SearchLocation,
    },
    store::{Archive, Snapshot},
    wire::Id,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

const VECTOR_BATCH: usize = 16;
const MAX_MEDIA_PARTS: usize = 48;

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
        let key = model.id().to_string();
        let mut cursor = vector_cursor(&self.db, head, &key)?;
        while budget != 0 {
            let pending = self.pending_vectors(head, cursor, budget.min(VECTOR_BATCH))?;
            if pending.is_empty() {
                break;
            }
            let indexed = self.embed_passages(snapshot, &pending, model)?;
            let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
            {
                let mut insert = tx.prepare_cached(
                    "INSERT OR IGNORE INTO rep_search_vectors(head,model,passage,part,span,vector)
                     SELECT ?1,?2,id,?4,?5,?6 FROM rep_search_passages WHERE id=?3",
                )?;
                let mut limited = tx.prepare_cached(
                    "INSERT OR IGNORE INTO rep_search_limited_media(head,model,passage)
                     SELECT ?1,?2,id FROM rep_search_passages WHERE id=?3",
                )?;
                for (passage, indexed) in pending.iter().zip(indexed) {
                    for (part, (span, vector)) in indexed.parts.into_iter().enumerate() {
                        let bytes: Vec<_> = vector.into_iter().flat_map(f32::to_le_bytes).collect();
                        let span = span.map(|span| serde_json::to_string(&span)).transpose()?;
                        insert.execute(params![head, key, passage.id, part as i64, span, bytes])?;
                    }
                    if indexed.limited {
                        limited.execute(params![head, key, passage.id])?;
                    }
                }
            }
            cursor = pending.last().context("empty embedding batch")?.id;
            // A withdrawal may remove queued passages while the model works.
            // Monotonic passage IDs prevent stale vectors from attaching to rebuilt rows.
            tx.execute(
                "INSERT INTO rep_search_vector_progress(head,model,cursor)
                 SELECT head,?2,?3 FROM rep_search_heads WHERE head=?1
                 ON CONFLICT(head,model) DO UPDATE SET cursor=max(cursor,excluded.cursor)",
                params![head, key, cursor],
            )?;
            tx.commit()?;
            budget -= pending.len();
        }
        Ok(())
    }

    fn pending_vectors(&self, head: &str, cursor: i64, limit: usize) -> Result<Vec<Pending>> {
        self.db
            .prepare_cached(
                "SELECT p.id,e.object,p.location,p.text FROM rep_search_passages p
             JOIN rep_search_entries e ON e.id=p.entry
             WHERE p.head=? AND p.semantic AND p.id>? ORDER BY p.id LIMIT ?",
            )?
            .query_map(params![head, cursor, limit as i64], |row| {
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
    ) -> Result<Vec<CachedVectors>> {
        let text: Vec<_> = pending
            .iter()
            .filter(|passage| !matches!(passage.location, SearchLocation::Media { .. }))
            .map(|passage| passage.text.as_str())
            .collect();
        let mut texts = if text.is_empty() {
            Vec::new()
        } else {
            text_embeddings(model, EmbeddingInput::Passage, &text)?
        }
        .into_iter();
        pending
            .iter()
            .map(|pending| {
                let SearchLocation::Media { media_type, .. } = &pending.location else {
                    let embedding = texts.next().context("missing text embedding")?;
                    return Ok(CachedVectors {
                        parts: vec![(None, embedding)],
                        limited: false,
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
                    return Ok(CachedVectors {
                        limited: true,
                        ..Default::default()
                    });
                }
                let embedded = media_embeddings(
                    model,
                    EmbeddingInput::Passage,
                    MediaInput {
                        media_type,
                        bytes: &bytes,
                    },
                )?;
                let parts = embedded
                    .parts
                    .into_iter()
                    .map(|part| (Some(part.span), part.vector))
                    .collect();
                Ok(CachedVectors {
                    parts,
                    limited: embedded.limited,
                })
            })
            .collect()
    }
}

fn vector_cursor(db: &Connection, head: &str, model: &str) -> Result<i64> {
    Ok(db
        .query_row(
            "SELECT cursor FROM rep_search_vector_progress WHERE head=? AND model=?",
            params![head, model],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

pub(super) fn missing_vectors(db: &Connection, head: &str, model: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM rep_search_passages WHERE head=? AND semantic AND id>?)",
        params![head, vector_cursor(db, head, model)?],
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
