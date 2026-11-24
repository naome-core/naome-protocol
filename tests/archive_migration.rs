#![cfg(feature = "cli")]
use anyhow::Result;
use naome_protocol::{
    profile::{PUBLIC, READ_ONLY},
    store::{Archive, Bundle},
    wire::{Format, Id},
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

#[test]
fn continuation_reuses_a_unique_full_tip_and_refuses_ambiguity() -> Result<()> {
    let a = Archive::memory()?;
    let first = a.snapshot_id("local", Default::default())?;
    assert_eq!(a.continue_snapshot("local")?, first);
    let raw = a.blob(b"second", Id::blob(PUBLIC))?;
    let second = a.continue_snapshot("local")?;
    assert_eq!(a.custody_head(second)?.parent, Some(first));
    assert_eq!(a.resolve_head(second, raw, "read")?, b"second");
    assert_eq!(a.continue_snapshot("local")?, second);
    a.custody_append(
        "local",
        Some(first),
        &[naome_protocol::store::custody::Item::blob(
            b"sibling".to_vec(),
            Id::blob(PUBLIC),
        )],
    )?;
    assert!(a.continue_snapshot("local").is_err());
    Ok(())
}

#[test]
fn migrates_stored_page_bytes_and_reads_during_a_writer_reservation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("archive.sqlite");
    let a = Archive::open(&file)?;
    for i in 0u32..150 {
        a.blob(&i.to_le_bytes(), Id::blob(PUBLIC))?;
    }
    let base = a.snapshot_id("lab", Default::default())?;
    let id = a.blob(b"one more", Id::blob(PUBLIC))?;
    let child = a.continue_snapshot("lab")?;
    drop(a);
    let sql = Connection::open(&file)?;
    sql.execute_batch(
        "BEGIN IMMEDIATE;
        ALTER TABLE custody_pages RENAME TO source_pages;
        CREATE TABLE custody_pages(id TEXT PRIMARY KEY,data BLOB NOT NULL) WITHOUT ROWID;
        INSERT INTO custody_pages SELECT * FROM source_pages;
        DROP TABLE source_pages; COMMIT;",
    )?;
    let stored = || -> rusqlite::Result<Vec<(String, Vec<u8>)>> {
        sql.prepare("SELECT id,data FROM custody_pages ORDER BY id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    };
    let before = stored()?;
    assert!(before.iter().any(|(_, bytes)| bytes.first() == Some(&3)));
    drop(Archive::open(&file)?);
    assert_eq!(before, stored()?);
    assert_eq!(
        sql.query_row(
            "SELECT wr FROM pragma_table_list WHERE name='custody_pages'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    sql.execute_batch("BEGIN IMMEDIATE")?;
    let reader = Archive::open_for_read(&file)?;
    assert_eq!(reader.resolve_head(child, id, "read")?, b"one more");
    assert!(reader.resolve_head(base, id, "read").is_err());
    sql.execute_batch("ROLLBACK")?;
    Ok(())
}

#[test]
fn cli_captures_sixteen_blocks_in_one_child_and_exchanges_a_single_member() -> Result<()> {
    use std::process::Command;
    let dir = tempfile::tempdir()?;
    let run = |db: &str, args: &[&str]| -> Result<serde_json::Value> {
        let out = Command::new(env!("CARGO_BIN_EXE_nao"))
            .current_dir(dir.path())
            .args(["--db", db])
            .args(args)
            .output()?;
        anyhow::ensure!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(serde_json::from_slice(&out.stdout)?)
    };
    let payload = b"stream descriptor";
    fs::write(dir.path().join("stream"), payload)?;
    let stream = Id::blob(payload);
    let root = run(
        "a.sqlite",
        &[
            "custody",
            "append",
            "lab",
            "--file",
            "stream",
            "--access",
            &Id::blob(READ_ONLY).to_string(),
            "--expected",
            &stream.to_string(),
        ],
    )?;
    let parent = root["head"].as_str().unwrap();
    let data: Vec<_> = (0u8..16)
        .flat_map(|i| std::iter::repeat_n(i, 256 * 1024))
        .collect();
    fs::write(dir.path().join("blocks"), &data)?;
    let captured = run(
        "a.sqlite",
        &[
            "custody",
            "capture",
            "lab",
            &stream.to_string(),
            "blocks",
            "--parent",
            parent,
        ],
    )?;
    let head = captured["head"].as_str().unwrap();
    let db = Archive::open_for_read(dir.path().join("a.sqlite"))?;
    let h = db.custody_head(head.parse()?)?;
    assert_eq!(h.parent, Some(parent.parse()?));
    assert_eq!(h.delta.len(), 16);
    assert!(h.delta.iter().all(|m| m.access == Id::blob(READ_ONLY)));
    assert!(
        db.custody_resolve(head.parse()?, h.delta[0].id, "train")
            .is_err()
    );
    assert_eq!(
        db.custody_range(head.parse()?, stream, 0, data.len())?,
        data
    );
    run(
        "a.sqlite",
        &[
            "export",
            head,
            "proof.json",
            "--member",
            &stream.to_string(),
        ],
    )?;
    assert_eq!(
        run("b.sqlite", &["import", "proof.json", "--member-head", head])?,
        serde_json::to_value(stream)?
    );
    let b = Archive::open_for_read(dir.path().join("b.sqlite"))?;
    let block = h.delta[0].id.to_string();
    let stream_id = stream.to_string();
    let exported = run(
        "a.sqlite",
        &[
            "export",
            head,
            "group.json",
            "--member",
            &stream_id,
            "--member",
            &block,
        ],
    )?;
    let imported = run("b.sqlite", &["import", "group.json", "--member-head", head])?;
    assert_eq!(imported, exported["members"]);
    let origin = Archive::open(dir.path().join("a.sqlite"))?;
    origin.custody_share(head.parse()?, true)?;
    let pull = run(
        "c.sqlite",
        &["custody", "pull", "a.sqlite", head, "--until-complete"],
    )?;
    assert_eq!(pull["cursor"], head);
    assert_eq!(pull["batches"], 2);
    let repeat = run(
        "c.sqlite",
        &["custody", "pull", "a.sqlite", head, "--until-complete"],
    )?;
    assert_eq!(repeat["batches"], 0);
    assert_eq!(b.resolve_head(head.parse()?, stream, "read")?, payload);
    assert!(b.head(head.parse()?).is_err());
    Ok(())
}

#[test]
fn bounded_discovery_matches_the_materialized_inventory_at_every_cursor() -> Result<()> {
    let db = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    for i in 0_u32..80 {
        db.blob(&i.to_le_bytes(), public)?;
        let d = naome_protocol::profile::Document::new(
            naome_protocol::profile::Kind::Claim,
            "point page",
            "test",
            serde_json::json!({"i":i}),
            vec![],
        )?;
        let format = if i % 2 == 0 { Format::A } else { Format::B };
        db.document(&d, format, public)?;
    }
    let snapshot = db.snapshot("page comparison", Default::default())?;
    let mut cursor = None;
    loop {
        let full = db.discover(&snapshot, None, Some("point page"), None, "read", 7, cursor)?;
        let page = db.discover_head(
            snapshot.id()?,
            None,
            Some("point page"),
            None,
            "read",
            7,
            cursor,
        )?;
        assert_eq!(serde_json::to_value(&full)?, serde_json::to_value(&page)?);
        cursor = page.after;
        if cursor.is_none() {
            break;
        }
    }
    Ok(())
}
