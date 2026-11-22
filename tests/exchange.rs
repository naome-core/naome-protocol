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
fn published_offline_exchange_matches_native_export_and_import() -> Result<()> {
    let example = Bundle::read(&include_bytes!("../spec/examples/exchange-v1.json")[..])?;
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
    let example = Bundle::read(&include_bytes!("../spec/examples/exchange-v1.json")[..])?;
    let bytes = serde_json::to_vec(&example)?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_rep"));
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
fn paged_exchange_converges_on_the_same_inventory() -> Result<()> {
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
