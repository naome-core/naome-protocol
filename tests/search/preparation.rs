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
    assert!(archive.prepare_search(&snapshot, 100, None)?.index_complete);
    claim(
        &archive,
        "child",
        "independent control",
        Id::blob(PUBLIC),
        Format::A,
    )?;
    let next = archive.snapshot("long reading", [snapshot.id()?].into())?;
    assert!(archive.prepare_search(&next, 100, None)?.index_complete);
    let chunk = archive.read(&snapshot, descriptor, "read")?.inputs[0].target;
    archive.withdraw(chunk)?;
    for cut in [&snapshot, &next] {
        assert!(archive.prepare_search(cut, 8, None)?.limited_coverage);
        let report = archive.search_prepared(cut, query, options, None)?;
        assert!(report.limited_coverage && report.hits.is_empty());
    }
    Ok(())
}

#[cfg(feature = "cli")]
#[test]
fn child_search_views_share_parent_entries_across_parent_rebuilds() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("search.sqlite");
    let a = Archive::open(&file)?;
    let public = Id::blob(PUBLIC);
    let base = claim(&a, "base", "common predecessor", public, Format::A)?;
    let root = a.snapshot("lab", Default::default())?;
    a.prepare_search(&root, 100, None)?;
    let left = claim(&a, "child", "first child", public, Format::A)?;
    let child = a.snapshot("lab", [root.id()?].into())?;
    a.prepare_search(&child, 100, None)?;
    let sql = rusqlite::Connection::open(file)?;
    assert_eq!(
        sql.query_row("SELECT count(*) FROM rep_search_set_entries", [], |r| r
            .get::<_, i64>(0))?,
        2
    );
    assert_eq!(
        sql.query_row("SELECT count(*) FROM rep_search_visible", [], |r| r
            .get::<_, i64>(0))?,
        3
    );
    a.clear_search_index(root.id()?)?;
    let result = a.search_prepared(
        &child,
        naome_protocol::search::SearchInput::Text("predecessor"),
        Default::default(),
        None,
    )?;
    assert_eq!(result.hits[0].id, base);
    a.prepare_search(&root, 100, None)?;
    assert!(
        a.find(&root, "child", Default::default(), None)?
            .hits
            .iter()
            .all(|hit| hit.id != left)
    );
    assert_eq!(
        a.find(&child, "first child", Default::default(), None)?
            .hits[0]
            .id,
        left
    );
    Ok(())
}

#[test]
fn positional_invalidation_respects_visibility_across_heads() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("views.sqlite");
    let a = Archive::open(&path)?;
    let public = Id::blob(PUBLIC);
    let left = claim(&a, "left", "left specimen", public, Format::A)?;
    let right = claim(&a, "right", "right specimen", public, Format::A)?;
    let head = |name, roots| a.selected_snapshot(name, BTreeSet::new(), roots);
    let first = head("left", [left].into())?;
    let second = head("right", [right].into())?;
    let both = head("both", [left, right].into())?;
    let mut model = TestEmbedder {
        id: Id::blob(b"positional scope"),
        embedded_passages: 0,
    };
    for view in [&first, &second, &both] {
        assert_eq!(
            a.prepare_search(view, 100, Some(&mut model))?
                .sample_positions_ready,
            Some(true)
        );
    }
    let db = rusqlite::Connection::open(path)?;
    let state = |id: Id| -> rusqlite::Result<bool> {
        db.query_row(
            "SELECT complete AND NOT invalid FROM rep_search_samples WHERE head=?",
            [id.to_string()],
            |r| r.get(0),
        )
    };
    assert!(state(first.id()?)? && state(second.id()?)? && state(both.id()?)?);
    db.execute("DELETE FROM rep_search_vectors WHERE passage IN (
        SELECT p.id FROM rep_search_passages p JOIN rep_search_entries e ON e.id=p.entry WHERE e.object=?)",[left.to_string()])?;
    assert!(!state(first.id()?)? && state(second.id()?)? && !state(both.id()?)?);
    Ok(())
}

#[test]
fn withdrawals_between_embedding_batches_exclude_later_passages() -> Result<()> {
    const WITHDRAWN: &str = "superseded independent text";
    struct WithdrawFirst {
        archive: std::path::PathBuf,
        chunk: Id,
        inner: TestEmbedder,
        batches: usize,
    }

    impl Embedder for WithdrawFirst {
        fn id(&self) -> Id {
            self.inner.id()
        }

        fn dimensions(&self) -> usize {
            self.inner.dimensions()
        }

        fn embed(&mut self, input: EmbeddingInput, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            if self.batches == 0 {
                Archive::open(&self.archive)?.withdraw(self.chunk)?;
            }
            self.batches += 1;
            assert!(texts.iter().all(|text| !text.contains(WITHDRAWN)));
            self.inner.embed(input, texts)
        }
    }

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("preparation.sqlite");
    let archive = Archive::open(&path)?;
    let public = Id::blob(PUBLIC);
    for n in 0..20 {
        claim(
            &archive,
            &format!("independent observation {n}"),
            "prepare before the later format",
            public,
            Format::A,
        )?;
    }
    let descriptor =
        archive.capture(WITHDRAWN.as_bytes(), "text/plain", 1024, public, Format::B)?;
    let snapshot = archive.snapshot("withdraw while preparing", BTreeSet::new())?;
    let chunk = archive.read(&snapshot, descriptor, "read")?.one("chunk")?;
    archive.prepare_search(&snapshot, 256, None)?;
    let mut model = WithdrawFirst {
        archive: path,
        chunk,
        inner: TestEmbedder {
            id: Id::blob(b"bounded semantic preparation"),
            embedded_passages: 0,
        },
        batches: 0,
    };
    let state = archive.prepare_search(&snapshot, 256, Some(&mut model))?;
    assert!(model.batches > 1);
    assert!(state.limited_coverage);
    assert_eq!(state.semantic_index_complete, Some(true));
    Ok(())
}
