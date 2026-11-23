use super::*;

const V012_HEAD: &str = "1:1:49f23061e494caa854be82e4fcd4d90fb2adbb0fae26098ea1d7d33b19f1301f";
const V012_CLAIM: &str = "1:0:272aaa50ba84dee45abd5db1e1c7f6d95421439cb2142b01c345a461e6dcf011";
const V012_NOTICE: &str = "2:0:79760dbdaf935b9446708ab2a401353a0eaf938e99d8d18c859fb2aa5ad3d1d2";
const V012_PAYLOAD: &str = "1:0:2d18e83105b029228dc48853c135f636a01d76e3f252ba414f69040ea7305df1";
const V012_EXCHANGE_SHA256: &str =
    "f770b5783b6187826ed997b7591920919fe757a6da3eb4d6c7df9f6e0141e290";

#[test]
fn invalid_cli_search_preserves_the_index_and_session_validation() -> Result<()> {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    let directory = tempfile::tempdir()?;
    let database = directory.path().join("archive.sqlite");
    let archive = Archive::open(&database)?;
    let id = claim(
        &archive,
        "Readings",
        "instrument calibration",
        Id::blob(PUBLIC),
        Format::A,
    )?;
    let snapshot = archive.snapshot("readings", BTreeSet::new())?;
    let nao = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nao"));
        command
            .current_dir(directory.path())
            .arg("--db")
            .arg(&database)
            .env_remove("NAOME_SEARCH_MODEL");
        command
    };
    let run = |args: &[&str]| nao().args(args).output();
    let connection = rusqlite::Connection::open(&database)?;
    assert!(
        !run(&["find", "calibration", "--limit", "0"])?
            .status
            .success()
    );
    let index_created: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='rep_search_schema')",
        [],
        |row| row.get(0),
    )?;
    assert!(!index_created);

    let id_argument = id.to_string();
    let direct = run(&["find", &id_argument, "--mode", "semantic"])?;
    assert!(direct.status.success());
    let direct: serde_json::Value = serde_json::from_slice(&direct.stdout)?;
    assert_eq!(direct["hits"][0]["id"], json!(id));
    assert_eq!(direct["hits"][0]["exact_id"], true);
    for flag in ["--prepare", "--rebuild-index"] {
        let result = run(&["find", &id_argument, flag])?;
        assert!(!result.status.success(), "{flag}");
        assert!(String::from_utf8_lossy(&result.stderr).contains("prepare-search"));
    }
    let index_created: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='rep_search_schema')",
        [],
        |row| row.get(0),
    )?;
    assert!(!index_created);

    let prepared = archive.prepare_search(&snapshot, 100, None)?;
    assert!(prepared.index_complete);
    let model = "missing model";
    for args in [
        &["find", "calibration", "--limit", "0", "--rebuild-index"][..],
        &["find", "calibration", "--model", model, "--rebuild-index"][..],
        &["find", &id_argument, "--rebuild-index"][..],
        &["prepare-search", "--index-budget", "0", "--rebuild-index"][..],
        &["prepare-search", "--model", model, "--rebuild-index"][..],
    ] {
        let result = run(args)?;
        assert!(!result.status.success(), "{args:?}");
        let report = archive.search_prepared(
            &snapshot,
            naome_protocol::search::SearchInput::Text("calibration"),
            SearchOptions::default(),
            None,
        )?;
        assert!(report.index_complete, "{args:?}");
        assert_eq!(report.hits.first().map(|hit| hit.id), Some(id), "{args:?}");
    }

    let mut session = nao()
        .arg("search-session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut input = session.stdin.take().expect("piped session input");
    writeln!(
        input,
        r#"{{"op":"find","query":"calibration","media_type":"text/plain"}}"#
    )?;
    writeln!(input, r#"{{"op":"find","query":"calibration"}}"#)?;
    drop(input);
    let session = session.wait_with_output()?;
    assert!(session.status.success());
    let output: Vec<serde_json::Value> = String::from_utf8(session.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<serde_json::Result<_>>()?;
    assert_eq!(output.len(), 2);
    assert_eq!(output[0]["error"], "a media type requires --input FILE");
    assert_eq!(output[1]["ok"]["hits"][0]["id"], json!(id));
    Ok(())
}

#[test]
fn released_v012_archive_upgrades_in_place_through_the_cli() -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::{fs, process::Command};

    // Created with the v0.1.2 CLI (c546fba6ed08403c045b3438fd7ad446de258c5f)
    // using put (both formats), capture (40-byte chunks), snapshot, and publish.
    let directory = tempfile::tempdir()?;
    let db = directory.path().join("upgraded.sqlite");
    fs::write(
        &db,
        include_bytes!("../fixtures/search-upgrade-v0.1.2.sqlite"),
    )?;
    let run = |args: &[&str]| -> Result<serde_json::Value> {
        let output = Command::new(env!("CARGO_BIN_EXE_nao"))
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
    let unsupported = Command::new(env!("CARGO_BIN_EXE_nao"))
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

    let second = run(&["snapshot", "newer inventory"])?;
    assert_eq!(run(&["search-head"])?, second);
    assert_eq!(run(&["search-head", V012_HEAD])?, json!(V012_HEAD));
    assert_eq!(run(&["find", "interferometer drift"])?["head"], V012_HEAD);
    assert_eq!(
        run(&[
            "prepare-search",
            "--lexical",
            "--head",
            second.as_str().unwrap()
        ])?["head"],
        second
    );
    assert_eq!(run(&["search-head", "--clear"])?, second);

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
fn preparation_adds_the_media_passage_index_to_an_existing_cache() -> Result<()> {
    use naome_protocol::search::SearchInput;
    use rusqlite::Connection;

    let directory = tempfile::tempdir()?;
    let database = directory.path().join("archive.sqlite");
    let archive = Archive::open(&database)?;
    let public = Id::blob(PUBLIC);
    let wanted = claim(
        &archive,
        "interferometer",
        "alignment verification",
        public,
        Format::A,
    )?;
    archive.capture(&b"audio bytes"[..], "audio/wav", 128, public, Format::B)?;
    let snapshot = archive.snapshot("prepared archive", BTreeSet::new())?;
    assert!(archive.prepare_search(&snapshot, 100, None)?.index_complete);
    drop(archive);

    let db = Connection::open(&database)?;
    db.execute_batch("DROP INDEX rep_search_media_passages")?;
    let index_exists = || -> rusqlite::Result<bool> {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='rep_search_media_passages')",
            [],
            |row| row.get(0),
        )
    };
    let passage_ids = || -> rusqlite::Result<Vec<i64>> {
        db.prepare("SELECT id FROM rep_search_passages ORDER BY id")?
            .query_map([], |row| row.get(0))?
            .collect()
    };
    let before = passage_ids()?;
    assert!(!before.is_empty());

    let archive = Archive::open(&database)?;
    let query = "interferometer";
    let options = SearchOptions::default();
    let old = archive.search_prepared(&snapshot, SearchInput::Text(query), options, None)?;
    assert!(old.index_complete && old.limited_coverage);
    assert_eq!(old.hits[0].id, wanted);
    assert!(!index_exists()?);

    let current = archive.find(&snapshot, query, options, None)?;
    assert!(index_exists()?);
    assert_eq!(passage_ids()?, before);
    assert_eq!(serde_json::to_value(current)?, serde_json::to_value(old)?);
    Ok(())
}

#[test]
fn previous_caches_are_rebuilt_without_changing_the_archive() -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::{
        fs,
        io::Write,
        process::{Command, Stdio},
    };

    // v2 was populated by v0.1.3 using prepare-search on the released v0.1.2 archive.
    for cached in [
        include_bytes!("../fixtures/search-cache-v1.sqlite").as_slice(),
        include_bytes!("../fixtures/search-cache-v2.sqlite").as_slice(),
    ] {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("archive.sqlite");
        fs::write(&database, cached)?;
        let mut session = Command::new(env!("CARGO_BIN_EXE_nao"))
            .arg("--db")
            .arg(&database)
            .arg("search-session")
            .env_remove("NAOME_SEARCH_MODEL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        session.stdin.take().unwrap().write_all(
            b"{\"op\":\"prepare_search\",\"lexical\":true}\n{\"op\":\"find\",\"query\":\"interferometer drift\"}\n",
        )?;
        let output = session.wait_with_output()?;
        assert!(output.status.success());
        let lines: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)?
            .lines()
            .map(serde_json::from_str)
            .collect::<serde_json::Result<_>>()?;
        assert_eq!(lines[0]["ok"]["index_complete"], true);
        assert_eq!(lines[1]["ok"]["index_version"], 5);
        assert_eq!(lines[1]["ok"]["hits"][0]["id"], V012_CLAIM);

        let archive = Archive::open(database)?;
        let head = archive.head(V012_HEAD.parse()?)?;
        assert_eq!(
            archive.releases()?.into_iter().collect::<Vec<_>>(),
            [head.id()?]
        );
        let packet = serde_json::to_vec(&archive.export(&head, "read")?)?;
        assert_eq!(hex::encode(Sha256::digest(packet)), V012_EXCHANGE_SHA256);
    }
    Ok(())
}

#[test]
fn released_v016_cache_recovers_missing_hits_without_changing_evidence() -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::{fs, process::Command};

    // Populated by v0.1.6 with capture, snapshot, publish, search-head, and
    // prepare-search. That release returned no hits for either expected term.
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("archive.sqlite");
    fs::write(
        &database,
        include_bytes!("../fixtures/search-cache-v3.sqlite"),
    )?;
    let head = "1:1:27f30cce4a93bfd6936aa43ad5844b39ff555463a4a8f486813260892edb306c";
    let compact = "1:0:0b246181f0a55b988fda676dd0eaa914b4294cf2020ab09f5c169002f7b39c9c";
    let reading = "2:0:291d0ebd275db569d9443a428e479bfd3866147e6d3d62bbd0d1f3234333c4b3";
    {
        let archive = Archive::open(&database)?;
        let snapshot = archive.search_head(None)?;
        let stale = archive.search_prepared(
            &snapshot,
            naome_protocol::search::SearchInput::Text("spectralmarker"),
            SearchOptions::default(),
            None,
        )?;
        assert!(!stale.index_complete && stale.hits.is_empty());
    }
    let run = |args: &[&str]| -> Result<serde_json::Value> {
        let output = Command::new(env!("CARGO_BIN_EXE_nao"))
            .arg("--db")
            .arg(&database)
            .current_dir(directory.path())
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

    for (query, expected) in [("spectralmarker", compact), ("calibrationmarker", reading)] {
        let report = run(&["find", query])?;
        assert_eq!(report["head"], head);
        assert_eq!(report["index_complete"], true);
        assert_eq!(report["hits"][0]["id"], expected, "{query}");
        assert_eq!(report["limited_coverage"], false, "{query}");
        assert_eq!(report["hits"][0]["location"]["type"], "payload");
    }
    fs::write(directory.path().join("query.txt"), "calibrationmarker")?;
    let report = run(&["find", "--input", "query.txt", "--media-type", "Text/Plain"])?;
    assert_eq!(report["hits"][0]["id"], reading);
    assert_eq!(run(&["heads"])?, json!([head]));
    run(&["export", head, "exchange.json"])?;
    assert_eq!(
        hex::encode(Sha256::digest(fs::read(
            directory.path().join("exchange.json")
        )?)),
        "2f6cedbf1f79592282b5ea55a68b363223eed1ce0123c6026c35a68ac01fba8b"
    );
    Ok(())
}

#[test]
fn offline_imports_preserve_search_context_across_reopen() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("search.sqlite");
    let mut local = Archive::open(&database)?;
    let mut mirror = Archive::memory()?;
    let peer = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let own = claim(&local, "latticeclear", "latticeclear", public, Format::A)?;
    let work = local.snapshot("local work", Default::default())?;
    let work_id = work.id()?;
    let draft = claim(&peer, "otherlab", "otherlab", public, Format::A)?;
    let first = peer.selected_snapshot("first peer", Default::default(), [draft].into())?;
    let first_id = first.id()?;
    let latest = peer.selected_snapshot("second peer", [first_id].into(), [draft].into())?;
    let latest_id = latest.id()?;
    let first_packet = peer.export(&first, "read")?;
    let mut stream = Vec::new();
    peer.export_stream(&latest, "read", &mut stream)?;
    for recipient in [&mut local, &mut mirror] {
        assert_eq!(recipient.import(&first_packet)?, first_id);
        assert_eq!(recipient.import_stream(stream.as_slice())?, latest_id);
    }
    assert_eq!(mirror.search_head(None)?.id()?, first_id);
    // A round trip of a local snapshot should not turn it into a remote default.
    let ours = local.export(&work, "read")?;
    assert_eq!(local.import(&ours)?, work_id);
    drop(local);

    let local = Archive::open(&database)?;
    let command = |args: &[&str]| -> Result<serde_json::Value> {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_nao"))
            .arg("--db")
            .arg(&database)
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
    let result = command(&["find", "latticeclear"])?;
    assert_eq!(result["head"], json!(work_id));
    assert_eq!(result["hits"][0]["id"], json!(own));
    assert_eq!(
        command(&["find", "otherlab", "--head", &latest_id.to_string()])?["head"],
        json!(latest_id)
    );

    local.set_search_head(Some(first_id))?;
    let new_local = local.snapshot("reconciled working inventory", [work_id, latest_id].into())?;
    assert_eq!(local.search_head(None)?.id()?, first_id);
    local.set_search_head(None)?;
    assert_eq!(local.search_head(None)?.id()?, new_local.id()?);
    Ok(())
}
