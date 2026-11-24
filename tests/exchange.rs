use anyhow::Result;
use naome_protocol::{
    kit,
    profile::{Document, Kind, PUBLIC, link},
    store::{Archive, Bundle},
    wire::{Format, Id, Record, Term},
};
use serde_json::json;
use std::collections::BTreeSet;

#[test]
fn textual_identifiers_accept_only_the_canonical_representation() -> Result<()> {
    let digest = "ab".repeat(32);
    for prefix in ["1:0", "1:1", "2:0"] {
        let canonical = format!("{prefix}:{digest}");
        let id: Id = canonical.parse()?;
        assert_eq!(id.to_string(), canonical);
        assert_eq!(serde_json::to_value(id)?, json!(canonical));
    }
    for invalid in [
        format!("01:0:{digest}"),
        format!("+1:0:{digest}"),
        format!("1:00:{digest}"),
        format!("2:1:{digest}"),
        format!("1:0:{}", digest.to_ascii_uppercase()),
        format!("1:0:{digest}:"),
        format!("1:0:{}", &digest[..62]),
    ] {
        assert!(invalid.parse::<Id>().is_err(), "{invalid}");
    }
    Ok(())
}

#[test]
fn published_offline_exchange_matches_native_export_and_import() -> Result<()> {
    let example = Bundle::read(&include_bytes!("../spec/examples/exchange.json")[..])?;
    let source = Archive::memory()?;
    let id = source.blob(b"OK\n", Id::blob(PUBLIC))?;
    let head = source.selected_snapshot("bench", BTreeSet::new(), [id].into())?;
    assert_eq!(
        serde_json::to_value(source.export(&head, "read")?)?,
        serde_json::to_value(&example)?
    );

    let mut recipient = Archive::memory()?;
    let imported = recipient.import(&example)?;
    assert_eq!(imported, head.id()?);
    assert_eq!(
        recipient.resolve(&recipient.head(imported)?, id, "read")?,
        b"OK\n"
    );
    Ok(())
}

#[cfg(feature = "cli")]
#[test]
fn cli_export_preserves_exchange_bytes_and_existing_files() -> Result<()> {
    use std::{fs, process::Command};

    let directory = tempfile::tempdir()?;
    let db = directory.path().join("source.sqlite");
    let path = directory.path().join("exchange.json");
    let source = Archive::open(&db)?;
    let id = source.blob(b"OK\n", Id::blob(PUBLIC))?;
    let head = source.selected_snapshot("bench", BTreeSet::new(), [id].into())?;
    let example = Bundle::read(&include_bytes!("../spec/examples/exchange.json")[..])?;
    let bytes = serde_json::to_vec(&example)?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_nao"));
    command
        .arg("--db")
        .arg(&db)
        .arg("export")
        .arg(head.id()?.to_string())
        .arg(&path);
    let first = command.output()?;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(fs::read(&path)?, bytes);

    fs::write(&path, b"keep this file")?;
    assert!(!command.output()?.status.success());
    assert_eq!(fs::read(&path)?, b"keep this file");
    Ok(())
}

#[test]
fn both_kits_and_published_identifiers() -> Result<()> {
    for (f, len, digest) in [
        (
            Format::A,
            80,
            "48367bebac79d394b4e13f004988c1141c977b295574ceafd716f29e39732dab",
        ),
        (
            Format::B,
            237,
            "6e56a2da3546ca07d9e693089c0f240c72a754592e91abf7894a4cb413d7d390",
        ),
    ] {
        let cases = kit::test(kit::bytes(f), &kit::pin(f))?;
        assert_eq!(cases.len(), 6);
        assert!(cases.iter().all(|c| c.matches));
        assert!(kit::test(kit::bytes(f), "untrusted").is_err());
        let r = Record {
            kind: 0,
            profile: format!("1:1:{}", "00".repeat(32)).parse()?,
            body: Term::Uint(200),
            access: format!("1:1:{}", "00".repeat(32)).parse()?,
            edges: vec![],
            critical: vec![],
        };
        let bytes = r.encode(f);
        assert_eq!(bytes.len(), len);
        assert!(Id::record(f, &bytes).to_string().ends_with(digest));
        assert_eq!(Record::decode(&bytes, f)?, r);
        for n in [0, 23, 24, 255, 256, 65535, 65536, u32::MAX as u64, u64::MAX] {
            let t = Term::Uint(n);
            assert_eq!(Term::decode(&t.encode(f), f)?, t);
        }
        let t = Term::Text("line\n\u{0001}ß😀\u{2028}".into());
        assert_eq!(Term::decode(&t.encode(f), f)?, t);
    }
    Ok(())
}

#[test]
fn opaque_records_travel_partial_search_reports_frontier_and_withdrawal_survives_reimport()
-> Result<()> {
    let a = Archive::memory()?;
    let mut b = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let r = Record {
        kind: 42,
        profile: Id::blob(b"unpublished"),
        access: public,
        edges: vec![],
        critical: vec![99],
        body: Term::Uint(0),
    };
    let id = a.record(&r, Format::A)?;
    let blob = a.blob(b"retirable", public)?;
    let s = a.snapshot("archive", BTreeSet::new())?;
    let bundle = a.export(&s, "read")?;
    b.import(&bundle)?;
    assert_eq!(b.resolve(&s, id, "read")?, r.encode(Format::A));
    assert!(b.read(&s, id, "read").is_err());
    let limited = b.explain(&s, &[id], "read", 1, 10);
    assert!(limited.frontier.contains_key(&r.profile));
    let first = b.discover(&s, None, None, None, "read", 1, None)?;
    let second = b.discover(&s, None, None, None, "read", 1, first.after)?;
    assert_ne!(first.after, second.after);
    b.withdraw(blob)?;
    b.import(&bundle)?;
    assert!(b.resolve(&s, blob, "read").is_err());
    let mut broken = a.export(&s, "read")?;
    broken.packets.last_mut().unwrap().hex.push_str("01");
    let mut fresh = Archive::memory()?;
    assert!(fresh.import(&broken).is_err());
    assert!(fresh.head(s.id()?).is_err());
    assert!(
        !fresh
            .snapshot("empty", BTreeSet::new())?
            .objects
            .contains(&id)
    );
    Ok(())
}

#[test]
fn custody_exchange_converges_on_the_same_inventory() -> Result<()> {
    let a = Archive::memory()?;
    let mut b = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    for i in 0..8 {
        a.blob(&[i], public)?;
    }
    let s = a.snapshot("shared", BTreeSet::new())?;
    let mut after = None;
    let mut transferred = BTreeSet::new();
    loop {
        let page = a.export_page(&s, "read", after, 2)?;
        assert!(page.packets.len() <= 2);
        assert!(page.packets.iter().all(|p| transferred.insert(p.id)));
        b.import(&page)?;
        after = page.continuation;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(transferred, s.objects);
    for &id in &s.objects {
        assert_eq!(a.resolve(&s, id, "read")?, b.resolve(&s, id, "read")?);
    }
    Ok(())
}

#[test]
fn offline_selection_and_exchange_support_payloads_above_the_network_limit() -> Result<()> {
    use naome_protocol::store::sharing::MAX_MEMBER_BYTES;

    let source = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let bytes = vec![1; MAX_MEMBER_BYTES + 1];
    let selected = source.blob(&bytes, public)?;
    let unrelated = source.blob(b"unrelated", public)?;
    let snapshot = source.selected_snapshot("offline", BTreeSet::new(), [selected].into())?;
    assert!(!snapshot.objects.contains(&unrelated));

    let mut receiver = Archive::memory()?;
    let head = receiver.import(&source.export(&snapshot, "read")?)?;
    assert_eq!(
        receiver.resolve(&receiver.head(head)?, selected, "read")?,
        bytes
    );
    for archive in [&source, &receiver] {
        let err = archive.release(head).unwrap_err();
        assert!(err.to_string().contains("network limit"), "{err}");
        assert!(archive.observed_releases()?.is_empty());
    }
    Ok(())
}

#[test]
fn migration_exports_only_the_paired_inventories_and_mapping_evidence() -> Result<()> {
    for (source_format, target_format) in [(Format::A, Format::B), (Format::B, Format::A)] {
        let archive = Archive::memory()?;
        let public = Id::blob(PUBLIC);
        let raw = archive.blob(b"selected source bytes", public)?;
        let root = archive.document(
            &Document::new(
                Kind::Question,
                "fixture",
                "scope",
                json!({"question":"what?"}),
                vec![link("evidence", raw)],
            )?,
            source_format,
            public,
        )?;
        let source = archive.selected_snapshot("scoped source", BTreeSet::new(), [root].into())?;
        let unrelated = archive.blob(b"other local evidence", public)?;
        let other_root = archive.document(
            &Document::new(
                Kind::Question,
                "other",
                "scope",
                json!({"question":"unrelated"}),
                vec![link("evidence", unrelated)],
            )?,
            source_format,
            public,
        )?;

        let migration = archive.migrate(&source, target_format, "custodian")?;
        let exchange = archive.head(migration.exchange_head)?;
        assert_eq!(migration.pairs.len(), 1);
        assert_eq!(migration.pairs[&root].format(), Some(target_format));
        let bundle = archive.export(&exchange, "read")?;
        for unrelated in [unrelated, other_root] {
            assert!(!bundle.snapshot.objects.contains(&unrelated));
            assert!(!bundle.packets.iter().any(|packet| packet.id == unrelated));
            assert!(!bundle.omitted.contains(&unrelated));
        }
        let mut recipient = Archive::memory()?;
        let head = recipient.import(&bundle)?;
        let received = recipient.head(head)?;
        assert_eq!(
            recipient.verify_mapping(&received, migration.mapping)?,
            migration.pairs
        );
        assert_eq!(
            recipient.resolve(&received, raw, "read")?,
            b"selected source bytes"
        );
    }
    Ok(())
}

#[test]
fn unavailable_dependencies_are_retained_without_blocking_other_translations() -> Result<()> {
    let a = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let private = a.blob(br#"{"purposes":["custody"],"version":1}"#, public)?;
    let sealed = a.blob(b"restricted raw bytes", private)?;
    let scoped = a.document(
        &Document::new(
            Kind::Claim,
            "sample",
            "scope",
            json!({"predicate":"sealed claim"}),
            vec![link("raw", sealed)],
        )?,
        Format::A,
        public,
    )?;
    let free = a.document(
        &Document::new(
            Kind::Question,
            "public",
            "scope",
            json!({"purpose":"discover"}),
            vec![],
        )?,
        Format::A,
        public,
    )?;
    let s = a.snapshot("local", BTreeSet::new())?;
    let migrated = a.migrate(&s, Format::B, "custodian")?;
    assert!(migrated.retained.contains(&scoped));
    assert!(migrated.pairs.contains_key(&free));
    let pairs = a.verify_mapping(&a.head(migrated.exchange_head)?, migrated.mapping)?;
    assert!(!pairs.contains_key(&scoped));
    Ok(())
}

#[test]
fn streaming_exchange_preserves_identity_omissions_and_atomic_import() -> Result<()> {
    let source = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let private = source.blob(br#"{"purposes":["custody"],"version":1}"#, public)?;
    let withheld = source.blob(b"withheld content", private)?;
    let payload = source.blob(b"permitted content", public)?;
    let snapshot = source.snapshot("streaming", BTreeSet::new())?;
    let mut stream = Vec::new();
    let count = source.export_stream(&snapshot, "read", &mut stream)?;
    assert_eq!(count, source.export(&snapshot, "read")?.packets.len());
    let mut recipient = Archive::memory()?;
    assert!(
        recipient
            .import_stream(&stream[..stream.len() - 5])
            .is_err()
    );
    assert!(recipient.head(snapshot.id()?).is_err());
    assert!(
        recipient
            .selected_snapshot("probe", BTreeSet::new(), [payload].into())
            .is_err()
    );
    let head = recipient.import_stream(stream.as_slice())?;
    assert_eq!(head, snapshot.id()?);
    let received = recipient.head(head)?;
    assert_eq!(received.objects, snapshot.objects);
    assert_eq!(
        recipient.resolve(&received, payload, "read")?,
        b"permitted content"
    );
    assert!(recipient.resolve(&received, withheld, "read").is_err());
    assert_eq!(recipient.import_stream(stream.as_slice())?, head);
    Ok(())
}

#[test]
fn native_migration_rewrites_chains_with_repeated_dependencies() -> Result<()> {
    for (format, target) in [(Format::A, Format::B), (Format::B, Format::A)] {
        let archive = Archive::memory()?;
        let mut chain = Vec::new();
        for index in 0..64 {
            let inputs = chain.last().map_or_else(Vec::new, |&parent| {
                vec![link("source", parent), link("input", parent)]
            });
            let step = Document::new(
                Kind::Derivation,
                "trace",
                "review",
                json!({"step":index}),
                inputs,
            )?;
            chain.push(archive.document(&step, format, Id::blob(PUBLIC))?);
        }
        let snapshot = archive.snapshot("chain", Default::default())?;
        let migration = archive.migrate(&snapshot, target, "review")?;
        assert_eq!(migration.pairs.len(), chain.len());
        let exchange = archive.head(migration.exchange_head)?;
        assert_eq!(
            archive.verify_mapping(&exchange, migration.mapping)?,
            migration.pairs
        );
        let newest = archive.read(&exchange, migration.pairs[&chain[63]], "read")?;
        assert_eq!(newest.one("source")?, migration.pairs[&chain[62]]);
        assert_eq!(newest.one("input")?, migration.pairs[&chain[62]]);
    }
    Ok(())
}
