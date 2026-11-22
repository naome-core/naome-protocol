//! Ranked local retrieval over a fixed archive snapshot.

use crate::{profile::Kind, wire::Id};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[cfg(feature = "semantic-search")]
mod gemma;
#[cfg(feature = "semantic-search")]
pub use gemma::EmbeddingGemma;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
pub enum SearchMode {
    Lexical,
    Semantic,
    Hybrid,
}

/// The model identifier must change whenever the model, tokenizer or inference
/// configuration changes. Embeddings from different identifiers never mix.
pub trait Embedder {
    fn id(&self) -> Id;
    fn dimensions(&self) -> usize;
    /// Minimum cosine similarity for a retrieved passage.
    fn minimum_similarity(&self) -> f32 {
        0.65
    }
    fn embed(&mut self, input: EmbeddingInput, text: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Returns a vector per decoded media segment, with selectors into the original.
    fn embed_media(
        &mut self,
        _input: EmbeddingInput,
        _media: MediaInput<'_>,
    ) -> Result<EmbeddedMedia> {
        Ok(EmbeddedMedia {
            parts: Vec::new(),
            limited: true,
        })
    }
}

#[derive(Clone, Copy)]
pub enum EmbeddingInput {
    Query,
    Passage,
}

/// Maximum encoded media size per archive descriptor or query file.
pub const MAX_MEDIA_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy)]
pub enum SearchInput<'a> {
    Text(&'a str),
    TextFile(&'a str),
    Media(MediaInput<'a>),
}

#[derive(Clone, Copy)]
pub struct MediaInput<'a> {
    pub media_type: &'a str,
    pub bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaSpan {
    Whole,
    Time { start_ms: u64, end_ms: u64 },
    Page { number: u32 },
}

#[derive(Deserialize)]
pub struct EmbeddedMedia {
    pub parts: Vec<MediaPart>,
    pub limited: bool,
}

#[derive(Deserialize)]
pub struct MediaPart {
    pub span: MediaSpan,
    pub vector: Vec<f32>,
}

pub(crate) fn media_type(value: &str) -> &str {
    value.split(';').next().unwrap_or_default().trim()
}

pub fn text_media(value: &str) -> bool {
    let value = media_type(value);
    value.starts_with("text/")
        || value.ends_with("+json")
        || value.ends_with("+xml")
        || matches!(
            value,
            "application/json"
                | "application/x-ndjson"
                | "application/xml"
                | "application/javascript"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/sql"
                | "application/x-sh"
                | "application/x-httpd-php"
        )
}

pub fn native_media(value: &str) -> bool {
    matches!(
        media_type(value),
        "image/png"
            | "image/jpeg"
            | "image/webp"
            | "image/gif"
            | "image/tiff"
            | "audio/wav"
            | "audio/x-wav"
            | "audio/wave"
            | "audio/mpeg"
            | "audio/flac"
            | "audio/ogg"
            | "audio/mp4"
            | "audio/aac"
            | "audio/webm"
            | "video/mp4"
            | "video/quicktime"
            | "video/webm"
            | "video/x-matroska"
            | "application/pdf"
    )
}

/// Bounds apply per invocation. An unfinished index resumes on the next search.
#[derive(Clone, Copy)]
pub struct SearchOptions<'a> {
    pub mode: SearchMode,
    pub kind: Option<Kind>,
    pub subject: Option<&'a str>,
    pub scope: Option<&'a str>,
    pub limit: usize,
    pub index_budget: usize,
    pub vector_budget: usize,
}

impl Default for SearchOptions<'_> {
    fn default() -> Self {
        Self {
            mode: SearchMode::Lexical,
            kind: None,
            subject: None,
            scope: None,
            limit: 10,
            index_budget: 512,
            vector_budget: 50_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SearchLocation {
    Object,
    Field {
        pointer: String,
        start: usize,
        end: usize,
    },
    Payload {
        start: usize,
        end: usize,
    },
    Media {
        media_type: String,
        span: MediaSpan,
    },
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub id: Id,
    pub kind: Option<Kind>,
    pub subject: Option<String>,
    pub scope: Option<String>,
    pub media_type: Option<String>,
    pub location: SearchLocation,
    pub snippet: Option<String>,
    pub exact_id: bool,
    pub lexical_rank: Option<usize>,
    pub semantic_rank: Option<usize>,
    pub semantic_similarity: Option<f32>,
    pub links: Vec<crate::profile::Link>,
}

#[derive(Debug, Serialize)]
pub struct SearchPreparation {
    pub head: Id,
    pub index_complete: bool,
    pub semantic_index_complete: Option<bool>,
    pub limited_coverage: bool,
}

#[derive(Debug, Serialize)]
pub struct SearchReport {
    pub head: Id,
    pub mode: SearchMode,
    pub index_version: u32,
    pub model: Option<Id>,
    pub minimum_similarity: Option<f32>,
    pub index_complete: bool,
    pub semantic_index_complete: Option<bool>,
    pub vector_budget_exhausted: bool,
    pub vector_products: usize,
    pub limited_coverage: bool,
    pub query_limited: bool,
    pub hits: Vec<SearchHit>,
}
