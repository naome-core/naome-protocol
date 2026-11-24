use anyhow::Result;
use naome_protocol::{
    profile::{Document, Kind, PUBLIC, link},
    search::{Embedder, EmbeddingInput, SearchLocation, SearchMode, SearchOptions},
    store::Archive,
    wire::{Format, Id},
};
use serde_json::json;
use std::collections::BTreeSet;

#[cfg(feature = "cli")]
mod compatibility;
mod lexical;
mod preparation;
mod semantic;

fn claim(archive: &Archive, subject: &str, text: &str, access: Id, format: Format) -> Result<Id> {
    let document = Document::new(Kind::Claim, subject, "test", json!({"text": text}), vec![])?;
    archive.document(&document, format, access)
}

struct TestEmbedder {
    id: Id,
    embedded_passages: usize,
}

impl Embedder for TestEmbedder {
    fn id(&self) -> Id {
        self.id
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn embed(&mut self, input: EmbeddingInput, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if matches!(input, EmbeddingInput::Passage) {
            self.embedded_passages += texts.len();
        }
        Ok(texts
            .iter()
            .map(|text| {
                if text.contains("Vibration") || text.contains("振動") {
                    vec![1.0, 0.0]
                } else {
                    vec![0.0, 1.0]
                }
            })
            .collect())
    }
}
