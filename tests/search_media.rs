#![cfg(feature = "semantic-search")]

use anyhow::{Result, ensure};
use naome_protocol::{
    profile::PUBLIC,
    search::{
        EmbeddingGemma, MediaInput, MediaSpan, SearchInput, SearchLocation, SearchMode,
        SearchOptions,
    },
    store::Archive,
    wire::{Format, Id},
};
use std::{fs, path::PathBuf, time::Instant};

#[test]
#[ignore = "requires the local EmbeddingGemma 2 checkpoint and media test assets"]
fn embeddinggemma_searches_original_media_in_both_directions() -> Result<()> {
    let directory =
        PathBuf::from(std::env::var_os("NAOME_SEARCH_MODEL").expect("NAOME_SEARCH_MODEL"));
    let fixtures =
        PathBuf::from(std::env::var_os("NAOME_MEDIA_FIXTURES").expect("NAOME_MEDIA_FIXTURES"));
    let python =
        PathBuf::from(std::env::var_os("NAOME_SEARCH_PYTHON").unwrap_or_else(|| "python3".into()));
    let start = Instant::now();
    let mut model = EmbeddingGemma::open(&directory, &python)?;
    println!("Model load {:?}", start.elapsed());
    let archive = Archive::memory()?;
    let cases = [
        (
            "bridge.png",
            "image/png",
            "Golden Gate Bridge in San Francisco",
        ),
        ("roses.wav", "audio/wav", "Roses are red, violets are blue"),
        ("cat.mp4", "video/mp4", "a cat dozing"),
        (
            "nitrate.pdf",
            "application/pdf",
            "a plot of nitrate concentration in a river",
        ),
    ];
    let mut assets = Vec::new();
    for (file, media_type, query) in cases {
        let bytes = fs::read(fixtures.join(file))?;
        let id = archive.capture(
            bytes.as_slice(),
            media_type,
            128 * 1024,
            Id::blob(PUBLIC),
            Format::B,
        )?;
        assets.push((bytes, id, media_type, query));
    }
    let snap = archive.snapshot("media trial", Default::default())?;
    let options = SearchOptions {
        mode: SearchMode::Hybrid,
        ..Default::default()
    };
    for (bytes, id, media_type, query) in &assets {
        let start = Instant::now();
        let report = archive.find(&snap, query, options, Some(&mut model))?;
        println!(
            "{media_type} from text {:?}: {:?}",
            start.elapsed(),
            report
                .hits
                .iter()
                .map(|hit| (&hit.media_type, hit.semantic_similarity))
                .collect::<Vec<_>>()
        );
        ensure!(report.index_complete && report.semantic_index_complete == Some(true));
        let Some(hit) = report.hits.iter().take(3).find(|hit| hit.id == *id) else {
            anyhow::bail!("missing original {media_type} for text query {query}");
        };
        let SearchLocation::Media { span, .. } = hit.location else {
            anyhow::bail!("missing original media selector");
        };
        match *media_type {
            "image/png" => ensure!(span == MediaSpan::Whole),
            "application/pdf" => ensure!(span == MediaSpan::Page { number: 1 }),
            _ => ensure!(matches!(span, MediaSpan::Time { .. })),
        }
        let mut original = Vec::new();
        archive.materialize(&snap, hit.id, "read", 32 * 1024 * 1024, &mut original)?;
        ensure!(&original == bytes);
        let input = SearchInput::Media(MediaInput { media_type, bytes });
        let report = archive.find_input(&snap, input, options, Some(&mut model))?;
        let first = report.hits.first().expect("same media is a result");
        ensure!(first.id == *id, "matching input artifact should rank first");
    }
    Ok(())
}
