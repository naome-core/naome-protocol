#![cfg(feature = "cli")]
use anyhow::Result;
use naome_protocol::{
    profile::{PUBLIC, READ_ONLY},
    store::{Archive, Bundle},
    wire::Id,
};
use rusqlite::{Connection, params};
use std::{collections::BTreeSet, fs};

#[test]
fn historical_exchange_is_converted_once_and_reexported_with_authenticated_pages() -> Result<()> {
    let bundle = Bundle::read(include_bytes!("fixtures/flat-exchange.json").as_slice())?;
    let mut db = Archive::memory()?;
    let head = db.import(&bundle)?;
    assert_ne!(head, bundle.head);
    assert_eq!(db.import(&bundle)?, head);
    assert_eq!(db.current_head_id(bundle.head)?, head);
    let old = db.head(bundle.head)?;
    let current = db.head(head)?;
    assert_eq!(old.objects, current.objects);
    assert_eq!(db.export(&old, "read")?.head, head);
    let mut stream = Vec::new();
    db.export_stream(&old, "read", &mut stream)?;
    let mut recipient = Archive::memory()?;
    assert_eq!(recipient.import_stream(stream.as_slice())?, head);
    assert_eq!(
        recipient.resolve_head(head, Id::blob(b"OK\n"), "read")?,
        b"OK\n"
    );
    assert!(serde_json::to_vec(&old).is_err());
    Ok(())
}

#[test]
fn project_copy_preserves_large_objects_scopes_tombstones_and_source_bytes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source.sqlite");
    let target = dir.path().join("converted.sqlite");
    fs::write(
        &source,
        include_bytes!("fixtures/search-upgrade-v0.1.2.sqlite"),
    )?;
    let old = Connection::open(&source)?;
    let bytes = vec![0x61; 2 * 1024 * 1024 + 7];
    let payload = Id::blob(&bytes);
    let removed = Id::blob(b"retired");
    let public = Id::blob(PUBLIC);
    let acl = Id::blob(READ_ONLY);
    old.execute(
        "INSERT OR IGNORE INTO objects VALUES(?,?,?)",
        params![acl.to_string(), READ_ONLY, public.to_string()],
    )?;
    old.execute(
        "INSERT INTO objects VALUES(?,?,?)",
        params![payload.to_string(), bytes, acl.to_string()],
    )?;
    old.execute(
        "INSERT INTO objects VALUES(?,NULL,?)",
        params![removed.to_string(), public.to_string()],
    )?;
    let members: BTreeSet<_> = [public, acl, payload, removed].into();
    let raw = format!(
        r#"{{"version":1,"source":"retained","parents":[],"objects":{}}}"#,
        serde_json::to_string(&members)?
    )
    .into_bytes();
    let previous = Id::blob(&raw);
    old.execute(
        "INSERT INTO heads VALUES(?,?)",
        params![previous.to_string(), raw],
    )?;
    drop(old);
    let before = fs::read(&source)?;
    assert_eq!(Archive::migrate_archive_file(&source, &target)?, 2);
    assert_eq!(fs::read(&source)?, before);
    let db = Archive::open(&target)?;
    let head = db.current_head_id(previous)?;
    assert_ne!(head, previous);
    assert_eq!(db.head(previous)?.id()?, previous);
    assert_eq!(db.head(head)?.objects, members);
    assert_eq!(db.resolve_head(head, payload, "read")?, bytes);
    assert!(db.resolve_head(head, payload, "train").is_err());
    assert!(db.resolve_head(head, removed, "read").is_err());
    let sql = Connection::open(&target)?;
    assert_eq!(
        sql.query_row(
            "SELECT count(*) FROM archive_extent_blocks WHERE id=?",
            [payload.to_string()],
            |r| r.get::<_, i64>(0)
        )?,
        9
    );
    db.withdraw(payload)?;
    assert!(db.resolve_head(head, payload, "read").is_err());
    assert_eq!(
        sql.query_row(
            "SELECT count(*) FROM archive_extent_blocks WHERE id=?",
            [payload.to_string()],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    drop(sql);
    drop(db);
    let destination = fs::read(&target)?;
    assert!(Archive::migrate_archive_file(&source, &target).is_err());
    assert_eq!(fs::read(target)?, destination);
    assert_eq!(fs::read(source)?, before);
    Ok(())
}

#[test]
fn prerelease_lab_forks_retain_segments_receipt_and_exact_disclosure() -> Result<()> {
    // Created by the PR's previous writer at 78419e5.
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("forks.sqlite");
    let target = dir.path().join("current.sqlite");
    fs::write(&source, include_bytes!("fixtures/prerelease-forks.sqlite"))?;
    assert_eq!(Archive::migrate_archive_file(&source, &target)?, 3);
    let db = Archive::open(&target)?;
    let left = db.current_head_id(
        "1:1:793aff1cc40ed2f5330f11def652b2dfb0c20c6a156877b83ce4722b704edd07".parse()?,
    )?;
    let right = db.current_head_id(
        "1:1:9f6781a385568c482975362fe4ed39bdf6704bfeae97536e9cf423ea391e6f8e".parse()?,
    )?;
    let record: Id =
        "1:0:68adcf66760c10688b1baaa4d142b5289f0aa9804636a41926bdee3568edae87".parse()?;
    let receipt: Id =
        "1:1:21f89ac5e7b7e9ca39b88c18c43b73f12121f93a4b88ca8c91466b3483b57ea3".parse()?;
    receipt.verify(&db.custody_receipt(receipt)?)?;
    let tips: BTreeSet<_> = db.custody_tips("lab", None, 10)?.into_iter().collect();
    assert_eq!(tips, [left, right].into());
    let receiver = Archive::memory()?;
    assert!(receiver.custody_pull(&db, right, 8).is_err());
    assert!(receiver.custody_pull(&db, left, 8)?.complete);
    record.verify(&receiver.resolve_head(left, record, "read")?)?;
    assert!(
        receiver
            .resolve_head(left, Id::blob(b"second fork"), "read")
            .is_err()
    );
    Ok(())
}

#[test]
fn a_full_parent_seals_only_new_catalog_rows_and_withdrawal_starts_a_new_cut() -> Result<()> {
    let a = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let retained = a.blob(b"before", public)?;
    for n in 0u64..180 {
        a.blob(&n.to_le_bytes(), public)?;
    }
    let base = a.snapshot_id("lab", Default::default())?;
    let added = a.blob(b"after", public)?;
    let child = a.snapshot_id("lab", [base].into())?;
    let descriptor = a.custody_head(child)?;
    assert_eq!(descriptor.parent, Some(base));
    assert_eq!(descriptor.delta.len(), 1);
    assert_eq!(descriptor.delta[0].id, added);
    assert_eq!(a.snapshot_id("lab", [base].into())?, child);
    assert!(a.resolve_head(base, added, "read").is_err());
    assert_eq!(a.resolve_head(child, retained, "read")?, b"before");
    a.withdraw(added)?;
    let next = a.snapshot_id("lab", [child].into())?;
    assert!(!a.head(next)?.objects.contains(&added));
    assert!(a.head(child)?.objects.contains(&added));
    assert!(a.resolve_head(child, added, "read").is_err());
    Ok(())
}

#[test]
fn a_changed_historical_manifest_rolls_back_upgrade_and_copy_publication() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("changed.sqlite");
    let destination = dir.path().join("destination.sqlite");
    fs::write(
        &source,
        include_bytes!("fixtures/search-upgrade-v0.1.2.sqlite"),
    )?;
    let sql = Connection::open(&source)?;
    sql.execute(
        "UPDATE heads SET manifest=CAST(manifest || ' ' AS BLOB)",
        [],
    )?;
    drop(sql);
    let before = fs::read(&source)?;
    assert!(Archive::migrate_archive_file(&source, &destination).is_err());
    assert!(!destination.exists());
    assert_eq!(fs::read(&source)?, before);
    assert!(Archive::open(&source).is_err());
    let sql = Connection::open(&source)?;
    assert!(!sql.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='archive_layout')",
        [],
        |r| r.get::<_, bool>(0)
    )?);
    Ok(())
}

#[test]
fn cli_admits_and_transfers_a_large_single_logical_payload() -> Result<()> {
    use std::process::Command;
    let dir = tempfile::tempdir()?;
    let origin = dir.path().join("lab.sqlite");
    let data = vec![13; 5 * 1024 * 1024 + 3];
    let id = Id::blob(&data);
    fs::write(dir.path().join("large.bin"), &data)?;
    fs::write(
        dir.path().join("members.json"),
        serde_json::to_vec(&serde_json::json!([{
            "id":id,"access":Id::blob(PUBLIC),"file":"large.bin"
        }]))?,
    )?;
    let out = Command::new(env!("CARGO_BIN_EXE_nao"))
        .current_dir(dir.path())
        .arg("--db")
        .arg(&origin)
        .args(["custody", "append", "lab", "members.json"])
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let admitted: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let head: Id = serde_json::from_value(admitted["head"].clone())?;
    let source = Archive::open(origin)?;
    source.custody_share(head, true)?;
    let receiver = Archive::memory()?;
    let transfer = receiver.custody_pull(&source, head, 1)?;
    assert!(transfer.complete);
    assert_eq!(transfer.payload_bytes, data.len());
    assert_eq!(receiver.resolve_head(head, id, "read")?, data);
    Ok(())
}
