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
const V012_HEAD: &str = "1:1:49f23061e494caa854be82e4fcd4d90fb2adbb0fae26098ea1d7d33b19f1301f";
#[cfg(feature = "cli")]
const V012_CLAIM: &str = "1:0:272aaa50ba84dee45abd5db1e1c7f6d95421439cb2142b01c345a461e6dcf011";
#[cfg(feature = "cli")]
const V012_NOTICE: &str = "2:0:79760dbdaf935b9446708ab2a401353a0eaf938e99d8d18c859fb2aa5ad3d1d2";
#[cfg(feature = "cli")]
const V012_PAYLOAD: &str = "1:0:2d18e83105b029228dc48853c135f636a01d76e3f252ba414f69040ea7305df1";
#[cfg(feature = "cli")]
const V012_EXCHANGE_SHA256: &str =
    "f770b5783b6187826ed997b7591920919fe757a6da3eb4d6c7df9f6e0141e290";

fn claim(archive: &Archive, subject: &str, text: &str, access: Id, format: Format) -> Result<Id> {
    let document = Document::new(Kind::Claim, subject, "test", json!({"text": text}), vec![])?;
    archive.document(&document, format, access)
}

#[cfg(feature = "cli")]
#[test]
fn released_v012_archive_upgrades_in_place_through_the_cli() -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::{fs, process::Command};

    // Created by rep built from v0.1.2 (c546fba6ed08403c045b3438fd7ad446de258c5f)
    // using put (both formats), capture (40-byte chunks), snapshot, and publish.
    let directory = tempfile::tempdir()?;
    let db = directory.path().join("upgraded.sqlite");
    fs::write(&db, include_bytes!("fixtures/search-upgrade-v0.1.2.sqlite"))?;
    let run = |args: &[&str]| -> Result<serde_json::Value> {
        let output = Command::new(env!("CARGO_BIN_EXE_rep"))
            .arg("--db")
            .arg(&db)
            .args(args)
            .env_remove("NAOME_SEARCH_MODEL")
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    };
    assert_eq!(run(&["heads"])?, json!([V012_HEAD]));
    let unsupported = Command::new(env!("CARGO_BIN_EXE_rep"))
        .arg("--db")
        .arg(&db)
        .args(["find", "anything", "--mode", "hybrid"])
        .env_remove("NAOME_SEARCH_MODEL")
        .output()?;
    assert!(!unsupported.status.success());
    let report = run(&["find", "interferometer drift"])?;
    assert_eq!(report["head"], V012_HEAD);
    assert_eq!(report["hits"][0]["id"], V012_CLAIM);
    let query_file = directory.path().join("example.py");
    fs::write(&query_file, "# interferometer drift\n")?;
    assert_eq!(
        run(&["find", "--input", query_file.to_str().unwrap()])?["hits"][0]["id"],
        V012_CLAIM
    );
    assert_eq!(
        report["hits"][0]["location"]["pointer"],
        "/fields/statement"
    );
    let report = run(&["find", "deriva del instrumento", "--head", V012_HEAD])?;
    assert_eq!(report["hits"][0]["id"], V012_PAYLOAD);
    assert_eq!(report["hits"][0]["location"]["type"], "payload");
    assert_eq!(
        run(&["find", V012_NOTICE, "--mode", "semantic"])?["hits"][0]["exact_id"],
        true
    );
    assert_eq!(
        run(&["search", V012_HEAD, "--kind", "notice"])?["ids"],
        json!([V012_NOTICE])
    );

    let file = directory.path().join("exchange.json");
    run(&["export", V012_HEAD, file.to_str().unwrap()])?;
    assert_eq!(
        hex::encode(Sha256::digest(fs::read(file)?)),
        V012_EXCHANGE_SHA256
    );
    let archive = Archive::open(&db)?;
    let snapshot = archive.head(V012_HEAD.parse()?)?;
    let mut payload = Vec::new();
    archive.materialize(&snapshot, V012_PAYLOAD.parse()?, "read", 1024, &mut payload)?;
    assert_eq!(
        String::from_utf8(payload)?,
        "Informe de calibración. La estimación del desplazamiento se corrigió por la deriva del instrumento.\n"
    );
    Ok(())
}

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
fn partial_text_import_resumes_and_withdrawal_purges_dependent_postings() -> Result<()> {
    let origin = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let text = "獨特校準資料。測定結果の再評価。";
    let descriptor = origin.capture(
        text.as_bytes(),
        "text/plain; charset=utf-8",
        7,
        public,
        Format::B,
    )?;
    let snapshot = origin.snapshot("origin", BTreeSet::new())?;
    let document = origin.read(&snapshot, descriptor, "read")?;
    let chunk = document.inputs[1].target;
    let mut subset = origin.export(&snapshot, "read")?;
    subset.packets.retain(|packet| packet.id != chunk);
    subset.omitted.insert(chunk);
    let mut recipient = Archive::memory()?;
    let head = recipient.import(&subset)?;
    let snapshot = recipient.head(head)?;
    let query = "獨特校準資料";
    let partial = recipient.find(&snapshot, query, SearchOptions::default(), None)?;
    assert!(partial.index_complete && partial.limited_coverage && partial.hits.is_empty());
    recipient.import(&origin.export(&origin.head(head)?, "read")?)?;
    let complete = recipient.find(&snapshot, query, SearchOptions::default(), None)?;
    assert!(complete.index_complete && !complete.limited_coverage);
    let hit = &complete.hits[0];
    assert_eq!(hit.id, descriptor);
    let SearchLocation::Payload { start, end } = hit.location else {
        anyhow::bail!("expected verified payload byte range");
    };
    assert_eq!(hit.snippet.as_deref(), text.get(start..end));
    recipient.withdraw(chunk)?;
    let removed = recipient.find(&snapshot, query, SearchOptions::default(), None)?;
    assert!(removed.index_complete && removed.limited_coverage && removed.hits.is_empty());
    let metadata = recipient.find(&snapshot, "text/plain", SearchOptions::default(), None)?;
    assert_eq!(metadata.hits[0].id, descriptor);
    assert!(matches!(
        metadata.hits[0].location,
        SearchLocation::Field { .. }
    ));
    Ok(())
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

#[test]
fn hybrid_search_resumes_vector_index_and_reuses_only_matching_models() -> Result<()> {
    let archive = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let reading = claim(
        &archive,
        "振動",
        "振動の補正は温度変化の後に行われた。",
        public,
        Format::A,
    )?;
    let other = claim(
        &archive,
        "Unrelated",
        "Batch identifiers for shipping containers",
        public,
        Format::B,
    )?;
    let note = Document::new(
        Kind::Notice,
        "Vibration review",
        "test",
        json!({"reason": "Check the original reading"}),
        vec![link("corrects", reading)],
    )?;
    let note = archive.document(&note, Format::B, public)?;
    let snapshot = archive.snapshot("semantic", BTreeSet::new())?;
    let mut model = TestEmbedder {
        id: Id::blob(b"model one"),
        embedded_passages: 0,
    };
    let options = SearchOptions {
        mode: SearchMode::Hybrid,
        index_budget: 1,
        vector_budget: 2,
        ..Default::default()
    };
    assert!(archive.find(&snapshot, "Vibration", options, None).is_err());
    let first = archive.find(&snapshot, "Vibration", options, Some(&mut model))?;
    assert!(!first.index_complete && first.semantic_index_complete == Some(false));
    let options = SearchOptions {
        index_budget: 100,
        ..options
    };
    let complete = archive.find(&snapshot, "Vibration", options, Some(&mut model))?;
    assert!(complete.index_complete && complete.semantic_index_complete == Some(true));
    assert!(complete.vector_budget_exhausted);
    let options = SearchOptions {
        vector_budget: 100,
        ..options
    };
    let warm = archive.find(&snapshot, "Vibration", options, Some(&mut model))?;
    assert_eq!(warm.hits[0].id, note);
    assert_eq!(warm.hits[0].links[0].target, reading);
    assert!(
        warm.hits
            .iter()
            .any(|hit| hit.id == reading && hit.semantic_rank.is_some())
    );
    assert!(warm.hits.iter().all(|hit| hit.id != other));
    let embedded = model.embedded_passages;
    archive.find(&snapshot, "Vibration", options, Some(&mut model))?;
    assert_eq!(model.embedded_passages, embedded);
    let mut other_model = TestEmbedder {
        id: Id::blob(b"model two"),
        embedded_passages: 0,
    };
    let result = archive.find(&snapshot, "Vibration", options, Some(&mut other_model))?;
    assert_ne!(result.model, warm.model);
    assert_eq!(other_model.embedded_passages, embedded);
    Ok(())
}

#[cfg(feature = "cli")]
#[test]
fn withdrawal_during_embedding_cannot_restore_derived_payload() -> Result<()> {
    struct WithdrawOnEmbed {
        writer: Archive,
        chunk: Option<Id>,
        inner: TestEmbedder,
    }

    impl Embedder for WithdrawOnEmbed {
        fn id(&self) -> Id {
            self.inner.id()
        }
        fn dimensions(&self) -> usize {
            self.inner.dimensions()
        }
        fn embed(&mut self, input: EmbeddingInput, text: &[&str]) -> Result<Vec<Vec<f32>>> {
            if matches!(input, EmbeddingInput::Passage)
                && let Some(chunk) = self.chunk.take()
            {
                self.writer.withdraw(chunk)?;
            }
            self.inner.embed(input, text)
        }
    }

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("archive.sqlite");
    let archive = Archive::open(&path)?;
    let public = Id::blob(PUBLIC);
    let descriptor = archive.capture(
        "unicoderetrievalsecret Vibration".as_bytes(),
        "text/plain",
        1024,
        public,
        Format::A,
    )?;
    let snapshot = archive.snapshot("indexed", BTreeSet::new())?;
    let chunk = archive.read(&snapshot, descriptor, "read")?.inputs[0].target;
    assert_eq!(
        archive
            .find(
                &snapshot,
                "unicoderetrievalsecret",
                SearchOptions::default(),
                None
            )?
            .hits
            .len(),
        1
    );
    let mut model = WithdrawOnEmbed {
        writer: Archive::open(&path)?,
        chunk: Some(chunk),
        inner: TestEmbedder {
            id: Id::blob(b"test"),
            embedded_passages: 0,
        },
    };
    let options = SearchOptions {
        mode: SearchMode::Hybrid,
        ..Default::default()
    };
    let report = archive.find(
        &snapshot,
        "unicoderetrievalsecret Vibration",
        options,
        Some(&mut model),
    )?;
    assert!(report.limited_coverage && report.hits.is_empty());
    let db = rusqlite::Connection::open(&path)?;
    for query in [
        "SELECT count(*) FROM rep_search_terms WHERE term='unicoderetrievalsecret'",
        "SELECT count(*) FROM rep_search_vectors v LEFT JOIN rep_search_passages p ON p.id=v.passage WHERE p.id IS NULL",
    ] {
        assert_eq!(db.query_row(query, [], |row| row.get::<_, i64>(0))?, 0);
    }
    let bytes = std::fs::read(path)?;
    assert!(
        !bytes
            .windows(b"unicoderetrievalsecret".len())
            .any(|data| data == b"unicoderetrievalsecret")
    );
    Ok(())
}

#[test]
fn unified_search_accepts_media_queries_and_returns_resolvable_media_parts() -> Result<()> {
    use naome_protocol::search::{EmbeddedMedia, MediaInput, MediaPart, MediaSpan, SearchInput};

    struct SyntheticMedia;
    impl Embedder for SyntheticMedia {
        fn id(&self) -> Id {
            Id::blob(b"synthetic multimodal test")
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn embed(&mut self, _: EmbeddingInput, input: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(input
                .iter()
                .map(|text| {
                    if text.contains("bridge") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect())
        }
        fn embed_media(
            &mut self,
            _: EmbeddingInput,
            input: MediaInput<'_>,
        ) -> Result<EmbeddedMedia> {
            assert!(input.media_type == "video/mp4" || input.media_type == "image/png");
            let span = if input.media_type == "video/mp4" {
                MediaSpan::Time {
                    start_ms: 18_000,
                    end_ms: 25_000,
                }
            } else {
                MediaSpan::Whole
            };
            let vector = if input.bytes == b"bridge footage" {
                vec![1.0, 0.0]
            } else {
                vec![0.0, 1.0]
            };
            Ok(EmbeddedMedia {
                parts: vec![MediaPart { span, vector }],
                limited: false,
            })
        }
    }

    let archive = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let original = b"bridge footage";
    let video = archive.capture(original.as_slice(), "video/mp4", 5, public, Format::B)?;
    let dataset = archive.capture(
        br#"{"monitoring_site":"bridge","channel":"strain"}"#.as_slice(),
        "application/json",
        11,
        public,
        Format::A,
    )?;
    let snapshot = archive.snapshot("multimodal evidence", BTreeSet::new())?;
    let options = SearchOptions {
        mode: SearchMode::Hybrid,
        ..Default::default()
    };
    let mut model = SyntheticMedia;
    let report = archive.find(&snapshot, "bridge", options, Some(&mut model))?;
    assert_eq!(report.semantic_index_complete, Some(true));
    let hit = report.hits.iter().find(|hit| hit.id == video).unwrap();
    assert_eq!(hit.media_type.as_deref(), Some("video/mp4"));
    assert_eq!(
        hit.location,
        SearchLocation::Media {
            media_type: "video/mp4".to_owned(),
            span: MediaSpan::Time {
                start_ms: 18_000,
                end_ms: 25_000
            },
        }
    );
    assert!(
        report
            .hits
            .iter()
            .any(|hit| hit.id == dataset && matches!(hit.location, SearchLocation::Payload { .. }))
    );

    let input = SearchInput::Media(MediaInput {
        media_type: "image/png",
        bytes: original,
    });
    let report = archive.find_input(&snapshot, input, options, Some(&mut model))?;
    assert!(report.hits.iter().any(|hit| hit.id == video));
    let mut recovered = Vec::new();
    archive.materialize(&snapshot, video, "read", 1000, &mut recovered)?;
    assert_eq!(recovered, original);
    let chunk = archive.read(&snapshot, video, "read")?.inputs[0].target;
    archive.withdraw(chunk)?;
    let report = archive.find_input(&snapshot, input, options, Some(&mut model))?;
    assert!(report.limited_coverage && report.hits.iter().all(|hit| hit.id != video));
    Ok(())
}
