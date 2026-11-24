use super::*;

#[test]
fn authorized_snapshot_coverage_controls_results_and_stable_ranking() -> Result<()> {
    let archive = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let denied = archive.blob(br#"{"purposes":["train"],"version":1}"#, public)?;
    let allowed = claim(
        &archive,
        "Calibrated interferometer",
        "The drift correction came from the thermal report.",
        public,
        Format::A,
    )?;
    let secret = claim(
        &archive,
        "Thermal report",
        "drift correction drift correction",
        denied,
        Format::B,
    )?;
    let snapshot = archive.snapshot("initial", BTreeSet::new())?;
    let query = "drift correction";
    let first = archive.find(&snapshot, query, SearchOptions::default(), None)?;
    assert!(first.index_complete && first.limited_coverage);
    assert_eq!(
        first.hits.iter().map(|item| item.id).collect::<Vec<_>>(),
        [allowed]
    );
    assert!(
        archive
            .find(
                &snapshot,
                &secret.to_string(),
                SearchOptions::default(),
                None
            )?
            .hits
            .is_empty()
    );

    let outside = claim(
        &archive,
        "drift correction",
        "drift correction",
        public,
        Format::B,
    )?;
    let unchanged = archive.find(
        &snapshot,
        query,
        SearchOptions {
            index_budget: 1,
            ..Default::default()
        },
        None,
    )?;
    assert_eq!(
        serde_json::to_value(&first)?,
        serde_json::to_value(unchanged)?
    );
    let latest = archive.snapshot("new", [snapshot.id()?].into())?;
    assert_eq!(archive.search_head(None)?.id()?, latest.id()?);
    assert_eq!(
        archive
            .find(&latest, query, SearchOptions::default(), None)?
            .hits[0]
            .id,
        outside
    );
    Ok(())
}

#[test]
fn fresh_index_finds_whole_words_across_passage_boundaries() -> Result<()> {
    let archive = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let prefix = "x".repeat(742);
    let text = format!(r#"{{"padding":"{prefix}","label":"spectralmarker"}}"#);
    let marker = text.find("spectralmarker").unwrap();
    assert_eq!(marker, 765);
    let expected = archive.capture(text.as_bytes(), "application/json", 40, public, Format::A)?;
    let snapshot = archive.snapshot("compact JSON", BTreeSet::new())?;

    let report = archive.find(&snapshot, "spectralmarker", SearchOptions::default(), None)?;
    assert!(report.index_complete && !report.limited_coverage);
    assert_eq!(
        report.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        [expected]
    );
    let hit = &report.hits[0];
    let SearchLocation::Payload { start, end } = hit.location else {
        panic!("expected a payload selector");
    };
    assert!(start <= marker && end >= marker + "spectralmarker".len());
    assert_eq!(hit.snippet.as_deref(), Some(&text[start..end]));
    Ok(())
}

#[test]
fn lexical_search_keeps_long_words_across_capture_and_index_windows() -> Result<()> {
    let archive = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let token = format!("{}123", "longword".repeat(112));
    let limit = "x".repeat(4096);

    for start in [2047, 65_500] {
        let text = format!(
            "{}{token} {limit} {}",
            " ".repeat(start),
            " ".repeat(start * 4)
        );
        let expected = archive.capture(text.as_bytes(), "text/plain", 4096, public, Format::B)?;
        let snapshot =
            archive.selected_snapshot("long tokens", BTreeSet::new(), [expected].into())?;
        for _ in 0..3 {
            archive.prepare_search(&snapshot, 64, None)?;
        }
        for word in [&token, &limit] {
            let report = archive.find(&snapshot, word, SearchOptions::default(), None)?;
            assert!(report.index_complete && !report.limited_coverage);
            let hit = &report.hits[0];
            assert_eq!(hit.id, expected);
            let SearchLocation::Payload { start, end } = hit.location else {
                panic!("expected a payload locator");
            };
            assert!(text[start..end].contains(word));
        }
    }

    for start in [0, 65_500] {
        let text = format!(
            "{}{} indexedcontrol {}",
            " ".repeat(start),
            "z".repeat(4097),
            " ".repeat(start * 4)
        );
        let expected = archive.capture(text.as_bytes(), "text/plain", 4096, public, Format::B)?;
        for name in ["oversized", "cached oversized"] {
            let snapshot = archive.selected_snapshot(name, BTreeSet::new(), [expected].into())?;
            for _ in 0..3 {
                archive.prepare_search(&snapshot, 64, None)?;
            }
            let report =
                archive.find(&snapshot, "indexedcontrol", SearchOptions::default(), None)?;
            assert!(report.index_complete && report.limited_coverage);
            assert_eq!(report.hits[0].id, expected);
        }
    }
    Ok(())
}

#[test]
fn text_queries_and_large_captures_use_later_passages() -> Result<()> {
    use naome_protocol::search::SearchInput;

    let archive = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let street = claim(&archive, "Straße", "ος", public, Format::A)?;
    let input = format!("{}STRASSE", "noise ".repeat(150));
    let mut bytes = b"earlyfield ".to_vec();
    bytes.resize(4095, b' ');
    bytes.extend_from_slice("測定".as_bytes());
    bytes.resize(270_000, b' ');
    bytes.extend_from_slice(b"laterfield");
    let payload = archive.capture(bytes.as_slice(), "text/plain", 4096, public, Format::B)?;
    let snapshot = archive.snapshot("long text", BTreeSet::new())?;
    let first = archive.prepare_search(&snapshot, 2, None)?;
    assert!(!first.index_complete);
    let second = archive.prepare_search(&snapshot, 1, None)?;
    assert!(!second.index_complete);
    let early = archive.search_prepared(
        &snapshot,
        SearchInput::Text("earlyfield"),
        SearchOptions::default(),
        None,
    )?;
    assert_eq!(early.hits[0].id, payload);
    assert_eq!(early.hits[0].snippet.as_deref(), Some("earlyfield"));
    let third = archive.prepare_search(&snapshot, 4, None)?;
    assert!(third.index_complete && !third.limited_coverage);
    let late = archive.find(&snapshot, "laterfield", SearchOptions::default(), None)?;
    assert_eq!(late.hits[0].snippet.as_deref(), Some("laterfield"));
    assert_eq!(
        late.hits[0].location,
        SearchLocation::Payload {
            start: 270_000,
            end: 270_010
        }
    );
    let cross_chunk = archive.find(&snapshot, "測定", SearchOptions::default(), None)?;
    assert_eq!(cross_chunk.hits[0].id, payload);
    let file = archive.find_input(
        &snapshot,
        SearchInput::TextFile(&input),
        SearchOptions::default(),
        None,
    )?;
    assert_eq!(file.hits[0].id, street);
    assert!(!file.query_limited);
    assert_eq!(
        archive
            .find(&snapshot, "ΟΣ", SearchOptions::default(), None)?
            .hits[0]
            .id,
        street
    );
    Ok(())
}
