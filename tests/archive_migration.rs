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

#[test]
fn continuation_follows_a_rebuilt_full_seal_but_rejects_a_real_fork() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("archive.sqlite");
    let a = Archive::open(&file)?;
    let public = Id::blob(PUBLIC);
    let removed = a.blob(b"removed", public)?;
    let old = a.snapshot_id("local", Default::default())?;
    a.withdraw(removed)?;
    let rebuilt = a.continue_snapshot("local")?;
    assert_eq!(a.custody_head(rebuilt)?.parent, None);
    assert_eq!(a.head(rebuilt)?.parents, [old].into());
    assert!(!a.head(rebuilt)?.objects.contains(&removed));
    drop(a);
    let a = Archive::open(&file)?;
    assert_eq!(a.continue_snapshot("local")?, rebuilt);
    let added = a.blob(b"next", public)?;
    let next = a.continue_snapshot("local")?;
    assert_eq!(a.custody_head(next)?.parent, Some(rebuilt));
    assert_eq!(a.resolve_head(next, added, "read")?, b"next");
    a.snapshot_id("local", [old].into())?;
    assert!(a.continue_snapshot("local").is_err());
    Ok(())
}

fn flat_archive(path: &std::path::Path) -> Result<Connection> {
    let db = Connection::open(path)?;
    db.execute_batch(
        "CREATE TABLE objects(id TEXT PRIMARY KEY, data BLOB, access TEXT NOT NULL);
         CREATE TABLE heads(id TEXT PRIMARY KEY, manifest BLOB NOT NULL);
         CREATE TABLE releases(head TEXT PRIMARY KEY REFERENCES heads(id));
         CREATE TABLE suspended_releases(head TEXT PRIMARY KEY REFERENCES releases(head));",
    )?;
    for bytes in [PUBLIC, READ_ONLY, naome_protocol::profile::PROFILE] {
        db.execute(
            "INSERT INTO objects VALUES(?,?,?)",
            params![
                Id::blob(bytes).to_string(),
                bytes,
                Id::blob(PUBLIC).to_string()
            ],
        )?;
    }
    Ok(db)
}

fn flat_head(
    db: &Connection,
    source: &str,
    parents: &BTreeSet<Id>,
    objects: &BTreeSet<Id>,
) -> Result<(Id, Vec<u8>)> {
    let bytes = format!(
        r#"{{"version":1,"source":{},"parents":{},"objects":{}}}"#,
        serde_json::to_string(source)?,
        serde_json::to_string(parents)?,
        serde_json::to_string(objects)?,
    )
    .into_bytes();
    let id = Id::blob(&bytes);
    db.execute(
        "INSERT INTO heads VALUES(?,?)",
        params![id.to_string(), &bytes],
    )?;
    Ok((id, bytes))
}

#[test]
fn migration_preserves_a_flat_review_with_more_than_64_parents() -> Result<()> {
    use naome_protocol::store::sharing::MAX_MANIFEST_PARENTS;

    let dir = tempfile::tempdir()?;
    let source = dir.path().join("v017.sqlite");
    let target = dir.path().join("current.sqlite");
    let old = flat_archive(&source)?;
    let members = [Id::blob(PUBLIC)].into();
    let mut parents = BTreeSet::new();
    for n in 0..=MAX_MANIFEST_PARENTS {
        let (parent, _) = flat_head(
            &old,
            &format!("review origin {n}"),
            &BTreeSet::new(),
            &members,
        )?;
        parents.insert(parent);
    }
    let (review, bytes) = flat_head(&old, "local replica review", &parents, &members)?;
    drop(old);

    Archive::migrate_archive_file(&source, &target)?;
    let archive = Archive::open(&target)?;
    let historical = archive.head(review)?;
    assert_eq!(historical.parents, parents);
    assert_eq!(historical.objects, members);
    let pin = archive.deposit_manifest(&historical)?;
    assert_eq!(pin, review);
    let current = archive.current_head_id(review)?;
    assert_ne!(current, review);
    assert_eq!(archive.head(current)?.objects, members);
    let internal = archive
        .head(current)?
        .parents
        .difference(&parents)
        .copied()
        .next()
        .expect("a bounded ancestry group");
    let mut origins = BTreeSet::new();
    let mut pending = vec![current];
    while let Some(head) = pending.pop() {
        if parents.contains(&head) {
            origins.insert(head);
        } else {
            let node = archive.head(head)?;
            assert!((1..=MAX_MANIFEST_PARENTS).contains(&node.parents.len()));
            pending.extend(&node.parents);
        }
    }
    assert_eq!(origins, parents);
    let lab = "legacy snapshot ancestry";
    assert!(archive.custody_tips(lab, None, 10)?.is_empty());
    let seal = archive.snapshot_id(lab, Default::default())?;
    assert_eq!(archive.custody_tips(lab, None, 10)?, vec![seal]);
    assert_eq!(archive.continue_snapshot(lab)?, seal);
    let sql = Connection::open(&target)?;
    assert_eq!(
        sql.query_row(
            "SELECT manifest FROM archive_historical WHERE id=?",
            [review.to_string()],
            |r| r.get::<_, Vec<u8>>(0)
        )?,
        bytes
    );

    // A draft migration could leave an index row for an otherwise addressable group.
    sql.execute(
        "INSERT INTO custody_tips VALUES(?,?)",
        params![lab, internal.to_string()],
    )?;
    drop(sql);
    drop(archive);
    let archive = Archive::open(&target)?;
    assert_eq!(archive.custody_tips(lab, None, 10)?, vec![seal]);
    assert_eq!(archive.continue_snapshot(lab)?, seal);
    assert_eq!(archive.head(internal)?.id()?, internal);
    Ok(())
}

#[test]
fn migration_keeps_legacy_release_history_and_catalogs_only_servable_heads() -> Result<()> {
    use naome_protocol::store::sharing::{MAX_MEMBER_BYTES, MAX_RELEASE_BYTES};

    let dir = tempfile::tempdir()?;
    let source = dir.path().join("v017.sqlite");
    let target = dir.path().join("current.sqlite");
    let old = flat_archive(&source)?;
    let public = Id::blob(PUBLIC);
    let mut members = BTreeSet::from([public]);
    let mut uncharged = MAX_RELEASE_BYTES - PUBLIC.len();
    for n in 0_u8..16 {
        let bytes = vec![n; uncharged.min(MAX_MEMBER_BYTES)];
        let id = Id::blob(&bytes);
        old.execute(
            "INSERT INTO objects VALUES(?,?,?)",
            params![id.to_string(), &bytes, public.to_string()],
        )?;
        uncharged -= bytes.len();
        members.insert(id);
    }
    assert_eq!(uncharged, 0);
    let (large, _) = flat_head(&old, "old release at the limit", &BTreeSet::new(), &members)?;
    let (small, _) = flat_head(
        &old,
        "old release within the limit",
        &BTreeSet::new(),
        &[public].into(),
    )?;
    let withdrawn = Id::blob(b"withdrawn legacy member");
    old.execute(
        "INSERT INTO objects VALUES(?,NULL,?)",
        params![withdrawn.to_string(), public.to_string()],
    )?;
    let (suspended, _) = flat_head(
        &old,
        "old suspended release",
        &BTreeSet::new(),
        &[public, withdrawn].into(),
    )?;
    for head in [large, small, suspended] {
        old.execute("INSERT INTO releases VALUES(?)", [head.to_string()])?;
    }
    old.execute(
        "INSERT INTO suspended_releases VALUES(?)",
        [suspended.to_string()],
    )?;
    drop(old);

    Archive::migrate_archive_file(&source, &target)?;
    let archive = Archive::open(&target)?;
    let current_large = archive.current_head_id(large)?;
    let current_small = archive.current_head_id(small)?;
    let current_suspended = archive.current_head_id(suspended)?;
    assert_eq!(archive.head(large)?.objects, members);
    assert_eq!(archive.head(current_large)?.objects, members);
    assert_eq!(
        archive.observed_releases()?,
        [current_large, current_small, current_suspended].into()
    );
    assert_eq!(archive.releases()?, [current_small].into());
    assert_eq!(archive.catalog_state()?.available, 1);
    assert_eq!(archive.release_page(None)?.heads, vec![current_small]);
    assert_eq!(
        archive
            .announcements_after(0)?
            .into_iter()
            .map(|(_, head)| head)
            .collect::<Vec<_>>(),
        vec![current_small]
    );
    assert_eq!(
        archive.released_manifest(current_small)?.id()?,
        current_small
    );
    assert!(archive.released_manifest(current_large).is_err());
    assert!(archive.release(large).is_err());
    Ok(())
}

#[test]
fn migration_suspends_a_release_with_an_invalid_linked_inventory() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("v017.sqlite");
    let target = dir.path().join("current.sqlite");
    let old = flat_archive(&source)?;
    let public = Id::blob(PUBLIC);
    let (healthy, _) = flat_head(&old, "one member", &BTreeSet::new(), &[public].into())?;
    let scratch = Archive::memory()?;
    let single = scratch.seal_members("one member", BTreeSet::new(), [public].into())?;
    let mut invalid = single.manifest()?.clone();
    invalid.count += 1;
    let bytes = invalid.bytes()?;
    let link = Id::blob(&bytes);
    old.execute(
        "INSERT INTO objects VALUES(?,?,?)",
        params![link.to_string(), &bytes, public.to_string()],
    )?;
    let members = [link, public].into();
    let (broken, _) = flat_head(&old, "legacy release", &BTreeSet::new(), &members)?;
    for head in [healthy, broken] {
        old.execute("INSERT INTO releases VALUES(?)", [head.to_string()])?;
    }
    drop(old);

    Archive::migrate_archive_file(&source, &target)?;
    let archive = Archive::open(&target)?;
    let available = archive.current_head_id(healthy)?;
    let suspended = archive.current_head_id(broken)?;
    let historical = archive.head(broken)?;
    assert_eq!(historical.objects, members);
    assert_eq!(archive.resolve(&historical, link, "read")?, bytes);
    assert_eq!(archive.head(suspended)?.objects, members);
    assert_eq!(archive.observed_releases()?, [available, suspended].into());
    assert_eq!(archive.releases()?, [available].into());
    assert_eq!(archive.release_page(None)?.heads, vec![available]);
    assert_eq!(archive.released_manifest(available)?.id()?, available);
    assert!(archive.released_manifest(suspended).is_err());
    let error = archive.release(suspended).unwrap_err();
    assert!(
        format!("{error:#}").contains("inventory count mismatch"),
        "{error:#}"
    );
    Ok(())
}
