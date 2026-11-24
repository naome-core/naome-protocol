use super::*;

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
    let video = archive.capture(original.as_slice(), "Video/MP4", 5, public, Format::B)?;
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
    assert_eq!(hit.media_type.as_deref(), Some("Video/MP4"));
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

#[test]
fn a_failed_media_embedding_preserves_other_hits_and_is_not_retried_per_query() -> Result<()> {
    use naome_protocol::search::{EmbeddedMedia, MediaInput, SearchInput};
    struct Failing {
        text: TestEmbedder,
        attempts: usize,
    }
    impl Embedder for Failing {
        fn id(&self) -> Id {
            self.text.id()
        }
        fn dimensions(&self) -> usize {
            self.text.dimensions()
        }
        fn embed(&mut self, input: EmbeddingInput, text: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.text.embed(input, text)
        }
        fn embed_media(&mut self, _: EmbeddingInput, _: MediaInput<'_>) -> Result<EmbeddedMedia> {
            self.attempts += 1;
            anyhow::bail!("temporary encoder interruption")
        }
    }
    let archive = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let id = claim(&archive, "Vibration", "Vibration report", public, Format::B)?;
    let image = archive.capture(&b"image fixture"[..], "image/png", 10, public, Format::A)?;
    let snapshot = archive.snapshot("media failure", BTreeSet::new())?;
    let mut model = Failing {
        text: TestEmbedder {
            id: Id::blob(b"interrupted model"),
            embedded_passages: 0,
        },
        attempts: 0,
    };
    let prepared = archive.prepare_search(&snapshot, 100, Some(&mut model))?;
    assert_eq!(prepared.semantic_index_complete, Some(false));
    assert!(prepared.limited_coverage);
    let options = SearchOptions {
        mode: SearchMode::Hybrid,
        ..Default::default()
    };
    for _ in 0..2 {
        let report = archive.search_prepared(
            &snapshot,
            SearchInput::Text("Vibration"),
            options,
            Some(&mut model),
        )?;
        assert_eq!(report.hits[0].id, id);
        assert!(report.hits.iter().all(|hit| hit.id != image));
    }
    archive.prepare_search(&snapshot, 100, Some(&mut model))?;
    assert_eq!(model.attempts, 1);
    Ok(())
}

#[test]
fn projected_search_finds_late_candidates_under_an_actual_comparison_budget() -> Result<()> {
    struct Projected;
    impl Embedder for Projected {
        fn id(&self) -> Id {
            Id::blob(b"comparison-count model")
        }
        fn dimensions(&self) -> usize {
            128
        }
        fn embed(&mut self, _: EmbeddingInput, text: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(text
                .iter()
                .map(|text| {
                    let mut v = vec![0.0; 128];
                    v[if text.contains("target") { 0 } else { 1 }] = 1.0;
                    v
                })
                .collect())
        }
    }
    let archive = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let mut ids = Vec::new();
    // A records precede B records in extraction order. With the old prefix scan,
    // the only three matching vectors were never examined on this budget.
    for i in 0..64 {
        ids.push(claim(
            &archive,
            &format!("Source {i}"),
            "unrelated calibration",
            public,
            Format::A,
        )?);
    }
    let wanted = claim(
        &archive,
        "target accession",
        "target measurement",
        public,
        Format::B,
    )?;
    let snapshot = archive.snapshot("projection trial", BTreeSet::new())?;
    let mut model = Projected;
    archive.prepare_search(&snapshot, 1024, Some(&mut model))?;
    let exact = SearchOptions {
        mode: SearchMode::Semantic,
        ..Default::default()
    };
    let reference = archive.find(&snapshot, "target", exact, Some(&mut model))?;
    assert_eq!(reference.hits[0].id, wanted);
    assert!(!reference.vector_budget_exhausted);
    let bounded = SearchOptions {
        vector_budget: 16,
        ..exact
    };
    let report = archive.find(&snapshot, "target", bounded, Some(&mut model))?;
    assert_eq!(report.hits[0].id, wanted);
    assert!(
        report.vector_budget_exhausted && report.vector_products <= bounded.vector_budget * 128
    );
    assert!(report.vector_products < reference.vector_products);
    assert!(ids.iter().all(|id| *id != wanted));
    Ok(())
}

#[test]
fn limited_semantic_results_survive_different_cache_histories() -> Result<()> {
    struct ExactSubject(String);
    impl Embedder for ExactSubject {
        fn id(&self) -> Id {
            Id::blob(b"cache-history test")
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn embed(&mut self, input: EmbeddingInput, values: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(values
                .iter()
                .map(|value| {
                    if matches!(input, EmbeddingInput::Query) || *value == self.0 {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect())
        }
    }
    let mut expected = None;
    for prewarm in [false, true] {
        let archive = Archive::memory()?;
        let mut records = Vec::new();
        for subject in ["first", "second", "third", "fourth", "fifth"] {
            let document = Document::new(Kind::Claim, subject, "test", json!({}), vec![])?;
            let id = archive.document(&document, Format::A, Id::blob(PUBLIC))?;
            records.push((id, subject));
        }
        records.sort_unstable();
        let mut model = ExactSubject(records[2].1.into());
        if prewarm {
            let subset =
                archive.selected_snapshot("subset", BTreeSet::new(), [records[4].0].into())?;
            archive.prepare_search(&subset, 100, Some(&mut model))?;
        }
        let snapshot = archive.snapshot("all", BTreeSet::new())?;
        archive.prepare_search(&snapshot, 100, Some(&mut model))?;
        let options = SearchOptions {
            mode: SearchMode::Semantic,
            vector_budget: 1,
            ..Default::default()
        };
        let result = archive.find(&snapshot, "query", options, Some(&mut model))?;
        assert_eq!(
            result.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            [records[2].0]
        );
        assert_eq!(result.vector_rows_loaded, 1);
        assert_eq!(result.vector_rows_examined, result.vector_rows_loaded);
        // All records are claims. This equivalent filter exercises the ordered scan.
        let filtered = archive.search_prepared(
            &snapshot,
            naome_protocol::search::SearchInput::Text("query"),
            SearchOptions {
                kind: Some(Kind::Claim),
                ..options
            },
            Some(&mut model),
        )?;
        assert_eq!(
            serde_json::to_value(&result.hits)?,
            serde_json::to_value(&filtered.hits)?
        );
        assert_eq!(result.vector_products, filtered.vector_products);
        assert!(filtered.vector_rows_examined > result.vector_rows_examined);
        if let Some(head) = expected {
            assert_eq!(result.head, head);
        }
        expected = Some(result.head);
    }
    Ok(())
}
