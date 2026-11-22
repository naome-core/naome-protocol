#![cfg(feature = "semantic-search")]

use anyhow::{Result, ensure};
use naome_protocol::{
    profile::{Document, Kind, PUBLIC},
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
    let mut captions = Vec::new();
    for (subject, statement) in [
        (
            "Bridge engineering report",
            "The Golden Gate Bridge is a red suspension bridge over the entrance to San Francisco Bay.",
        ),
        (
            "A poetry recording and transcript",
            "Roses are red, violets are blue. This recording contains a person reciting the opening line of the poem.",
        ),
        (
            "Animal observation protocol",
            "A domestic cat lies curled up and dozes quietly in the footage.",
        ),
        (
            "Field chemistry report",
            "The line plot shows the concentration of nitrate in river samples during hydrological monitoring.",
        ),
    ] {
        let document = Document::new(
            Kind::Claim,
            subject,
            "trial",
            serde_json::json!({"statement": statement}),
            vec![],
        )?;
        captions.push(archive.document(&document, Format::A, Id::blob(PUBLIC))?);
    }
    for subject in [
        "Geomagnetic survey",
        "Electron transport in a semiconductor",
        "Microplate sample preparation",
    ] {
        let document = Document::new(
            Kind::Claim,
            subject,
            "trial",
            serde_json::json!({"statement": subject}),
            vec![],
        )?;
        archive.document(&document, Format::A, Id::blob(PUBLIC))?;
    }
    let snap = archive.snapshot("media trial", Default::default())?;
    let options = SearchOptions {
        mode: SearchMode::Hybrid,
        ..Default::default()
    };
    for (index, (bytes, id, media_type, query)) in assets.iter().enumerate() {
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
        let report = archive.search_prepared(
            &snap,
            input,
            SearchOptions {
                kind: Some(Kind::Claim),
                ..options
            },
            Some(&mut model),
        )?;
        ensure!(
            report
                .hits
                .first()
                .is_some_and(|hit| hit.id == captions[index]),
            "input {media_type} should retrieve the distinct matching text record"
        );
        println!("file {media_type} -> its distinct report is rank one");
    }
    Ok(())
}

#[test]
#[ignore = "requires the local search Python media dependencies"]
fn audio_selectors_use_the_container_time_axis() -> Result<()> {
    let python = std::env::var_os("NAOME_SEARCH_PYTHON").unwrap_or_else(|| "python3".into());
    let output = std::process::Command::new(python)
        .args(["-B", "tests/fixtures/search_audio_timeline.py"])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
