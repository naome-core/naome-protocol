use super::*;

#[test]
fn preparation_reuses_objects_across_snapshots_and_keeps_queries_read_only() -> Result<()> {
    use naome_protocol::search::SearchInput;
    let archive = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let first_id = claim(
        &archive,
        "Vibration",
        "Vibration correction",
        public,
        Format::A,
    )?;
    let first = archive.snapshot("first", BTreeSet::new())?;
    let second = archive.snapshot("same contents", [first.id()?].into())?;
    let mut model = TestEmbedder {
        id: Id::blob(b"shared model"),
        embedded_passages: 0,
    };
    let options = SearchOptions {
        mode: SearchMode::Hybrid,
        ..Default::default()
    };

    let report = archive.search_prepared(
        &first,
        SearchInput::Text("Vibration"),
        options,
        Some(&mut model),
    )?;
    assert!(!report.index_complete && report.hits.is_empty());
    assert_eq!(model.embedded_passages, 0);
    let first_state = archive.prepare_search(&first, 100, Some(&mut model))?;
    assert_eq!(first_state.semantic_index_complete, Some(true));
    let first_count = model.embedded_passages;
    assert!(first_count > 0);
    assert_eq!(
        archive
            .prepare_search(&second, 100, Some(&mut model))?
            .semantic_index_complete,
        Some(true)
    );
    assert_eq!(model.embedded_passages, first_count);
    assert_eq!(
        archive
            .search_prepared(
                &second,
                SearchInput::Text("Vibration"),
                options,
                Some(&mut model)
            )?
            .hits[0]
            .id,
        first_id
    );

    let acl = archive.blob(br#"{"purposes":["read"],"version":1}"#, public)?;
    let protected = claim(&archive, "other", "Vibration", acl, Format::B)?;
    let full = archive.snapshot("with policy", [second.id()?].into())?;
    archive.prepare_search(&full, 100, Some(&mut model))?;
    let restricted = archive.seal_members(
        "without policy",
        [full.id()?].into(),
        full.objects
            .iter()
            .copied()
            .filter(|id| *id != acl)
            .collect(),
    )?;
    let result = archive.find(&restricted, "Vibration", options, Some(&mut model))?;
    assert!(result.limited_coverage);
    assert!(result.hits.iter().all(|hit| hit.id != protected));
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
fn withdrawing_a_large_text_chunk_discards_its_prepared_passages() -> Result<()> {
    use naome_protocol::search::SearchInput;

    let archive = Archive::memory()?;
    let marker = b"retiredreading";
    let offset = 140_000;
    let mut bytes = vec![b' '; 320 * 1024];
    bytes[offset..offset + marker.len()].copy_from_slice(marker);
    let descriptor = archive.capture(
        bytes.as_slice(),
        "text/plain",
        bytes.len(),
        Id::blob(PUBLIC),
        Format::B,
    )?;
    let snapshot = archive.snapshot("long reading", BTreeSet::new())?;
    let preparation = archive.prepare_search(&snapshot, 8, None)?;
    assert!(!preparation.index_complete);

    let query = SearchInput::Text("retiredreading");
    let options = SearchOptions::default();
    let report = archive.search_prepared(&snapshot, query, options, None)?;
    assert_eq!(report.hits.len(), 1);
    assert_eq!(report.hits[0].id, descriptor);
    assert_eq!(
        report.hits[0].location,
        SearchLocation::Payload {
            start: offset,
            end: offset + marker.len(),
        }
    );
    let chunk = archive.read(&snapshot, descriptor, "read")?.inputs[0].target;
    archive.withdraw(chunk)?;

    let preparation = archive.prepare_search(&snapshot, 8, None)?;
    assert!(preparation.limited_coverage);
    let report = archive.search_prepared(&snapshot, query, options, None)?;
    assert!(report.limited_coverage && report.hits.is_empty());
    Ok(())
}
