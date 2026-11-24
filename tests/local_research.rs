#![cfg(feature = "cli")]

use anyhow::{Result, ensure};
use naome_protocol::{
    fixture,
    profile::{Document, Kind, PUBLIC, READ_ONLY},
    store::Archive,
    wire::{Format, Id},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn invoke(archive: &Path, cwd: &Path, args: &[&str]) -> Result<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_nao"))
        .current_dir(cwd)
        .env("NAO_ARCHIVE", archive)
        .env_remove("NAOME_SEARCH_MODEL")
        .args(args)
        .output()?)
}

fn run(archive: &Path, cwd: &Path, args: &[&str]) -> Result<Value> {
    let output = invoke(archive, cwd, args)?;
    ensure!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn identifier(value: &Value) -> &str {
    value.as_str().expect("expected serialized identifier")
}

#[test]
fn one_archive_reports_pending_work_and_continues_the_full_snapshot() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let archive = root.join("archive.sqlite");
    let other = root.join("second session");
    fs::create_dir(&other)?;
    assert_eq!(run(&archive, root, &["status"])?["archive"], Value::Null);
    assert!(!archive.exists());

    fs::write(root.join("reading.txt"), "original signal")?;
    let original = run(&archive, root, &["capture", "reading.txt", "text/plain"])?;
    let first = run(&archive, root, &["snapshot", "research"])?;
    fs::write(other.join("reading.txt"), "revised measurement")?;
    let revised = run(&archive, &other, &["capture", "reading.txt", "text/plain"])?;
    assert!(!other.join("evidence.sqlite").exists());
    let before = fs::read(&archive)?;
    let status = run(&archive, &other, &["status"])?;
    assert_eq!(
        Path::new(identifier(&status["path"])).canonicalize()?,
        archive.canonicalize()?
    );
    assert_eq!(status["archive"]["search_head"], first);
    assert_eq!(status["archive"]["pending_objects"], 2);
    assert_eq!(fs::read(&archive)?, before);

    let selected = run(
        &archive,
        &other,
        &["snapshot", "research", "--root", identifier(&revised)],
    )?;
    assert_eq!(run(&archive, &other, &["search-head"])?, first);
    assert_eq!(
        run(
            &archive,
            &other,
            &[
                "find",
                identifier(&original),
                "--head",
                identifier(&selected)
            ]
        )?["hits"],
        json!([])
    );
    let next = run(&archive, &other, &["snapshot", "--continue"])?;
    assert_ne!(next, first);
    assert_eq!(
        run(&archive, &other, &["find", identifier(&revised)])?["hits"][0]["id"],
        revised
    );
    assert_eq!(
        run(&archive, &other, &["status"])?["archive"]["pending_objects"],
        0
    );
    let store = Archive::open_for_read(&archive)?;
    assert_eq!(
        store.custody_head(identifier(&next).parse()?)?.parent,
        Some(identifier(&first).parse()?)
    );
    let explicit = run(&archive, root, &["--db", "other.sqlite", "status"])?;
    let explicit_path = Path::new(identifier(&explicit["path"]));
    assert_eq!(
        explicit_path.parent().unwrap().canonicalize()?,
        root.canonicalize()?
    );
    assert_eq!(explicit_path.file_name().unwrap(), "other.sqlite");
    assert_eq!(explicit["archive"], Value::Null);
    run(&archive, root, &["snapshot", "other research"])?;
    let before = fs::read(&archive)?;
    let ambiguous = invoke(&archive, root, &["snapshot", "--continue"])?;
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("exactly one full local tip"));
    assert_eq!(fs::read(&archive)?, before);
    assert_eq!(
        run(&archive, root, &["snapshot", "research", "--continue"])?,
        next
    );
    assert!(!invoke(&archive, root, &["snapshot"])?.status.success());
    Ok(())
}

fn document(root: &Path, archive: &Path, value: Value) -> Result<Value> {
    fs::write(root.join("record.json"), serde_json::to_vec(&value)?)?;
    run(archive, root, &["put", "record.json"])
}

#[test]
fn linked_capture_recovers_a_question_and_its_declared_evidence() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let archive = root.join("archive.sqlite");
    let put = |kind, fields, inputs| {
        document(
            root,
            &archive,
            json!({
                "kind":kind,"subject":"finite sets","scope":"ordinal study","fields":fields,"inputs":inputs
            }),
        )
    };
    let question = put(
        "question",
        json!({"statement":"Does the finite set have a least member?"}),
        json!([]),
    )?;
    let other_question = put(
        "question",
        json!({"statement":"What is the greatest member?"}),
        json!([]),
    )?;
    let derivation = put(
        "derivation",
        json!({"kernel":"nat-order-zero-v1","steps":["member-zero","zero-le-every-nat","least-intro"]}),
        json!([]),
    )?;
    let claim = put(
        "claim",
        json!({"predicate":"least-element","domain":"finite-nat-sets-containing-zero"}),
        json!([{"role":"question","target":question}]),
    )?;
    let other_claim = put(
        "claim",
        json!({"predicate":"greatest-element"}),
        json!([{"role":"question","target":other_question}]),
    )?;
    let add_argument = |target: &Value| {
        put(
            "argument",
            json!({"method":"least-zero"}),
            json!([{"role":"claim","target":target},{"role":"proof","target":derivation}]),
        )
    };
    let argument = add_argument(&claim)?;
    let unrelated = add_argument(&other_claim)?;

    fs::write(root.join("evidence.txt"), "Retained scratch work")?;
    let plain = run(&archive, root, &["capture", "evidence.txt", "text/plain"])?;
    let linked = run(
        &archive,
        root,
        &[
            "capture",
            "evidence.txt",
            "text/plain",
            "--for",
            identifier(&derivation),
            "--note",
            "Independent derivation notes",
        ],
    )?;
    assert_eq!(linked["descriptor"], plain);
    assert_ne!(linked["occurrence"], plain);
    let policy = document(
        root,
        &archive,
        json!({"kind":"policy","subject":"local evaluator","scope":"research-policy-1","fields":{"keys":{}}}),
    )?;
    let first = run(&archive, root, &["snapshot", "research"])?;
    fs::write(
        root.join("query.json"),
        serde_json::to_vec(
            &json!({"target":claim,"arguments":[argument],"policy":policy,"budget":100}),
        )?,
    )?;
    let assessment = run(
        &archive,
        root,
        &["assess", identifier(&first), "query.json"],
    )?;
    let receipt = identifier(&assessment["id"]);
    let before = fs::read(&archive)?;
    assert_eq!(run(&archive, root, &["replay", receipt])?["matches"], true);
    assert!(
        !invoke(
            &archive,
            root,
            &["replay", receipt, "--head", identifier(&first)]
        )?
        .status
        .success()
    );
    assert_eq!(fs::read(&archive)?, before);
    let head = run(&archive, root, &["snapshot", "research", "--continue"])?;

    let question_args = ["question", identifier(&question), "--budget", "16"];
    let overview = run(&archive, root, &question_args)?;
    assert_eq!(overview["head"], head);
    assert_eq!(overview["limited_coverage"], false);
    let records = overview["records"].as_array().unwrap();
    let record = |id: &Value| records.iter().find(|record| &record["id"] == id);
    assert_eq!(
        record(&assessment["id"]).unwrap()["reported_standing"],
        "supported"
    );
    assert_eq!(
        record(&linked["occurrence"]).unwrap()["text"],
        "Independent derivation notes"
    );
    assert!(record(&plain).is_some());
    assert!(record(&other_question).is_none() && record(&unrelated).is_none());
    let limited = run(
        &archive,
        root,
        &["question", identifier(&question), "--budget", "1"],
    )?;
    assert_eq!(limited["limited_coverage"], true);
    assert!(limited["unscanned_records_from"].is_null());

    let store = Archive::open(&archive)?;
    for index in 0..128 {
        store.document(
            &Document::new(
                Kind::Question,
                "unrelated",
                "separate study",
                json!({"statement":format!("Independent question {index}")}),
                vec![],
            )?,
            Format::A,
            Id::blob(PUBLIC),
        )?;
    }
    drop(store);
    let expanded = run(&archive, root, &["snapshot", "--continue"])?;
    let expanded_overview = run(&archive, root, &question_args)?;
    assert_eq!(expanded_overview["head"], expanded);
    assert_eq!(expanded_overview["records"], overview["records"]);
    assert_eq!(expanded_overview["limited_coverage"], false);

    let store = Archive::open(&archive)?;
    store.document(
        &Document::new(
            Kind::Mapping,
            "unrelated mapping",
            "separate study",
            json!({}),
            vec![],
        )?,
        Format::A,
        Id::blob(PUBLIC),
    )?;
    drop(store);
    run(&archive, root, &["snapshot", "--continue"])?;
    let limited = run(&archive, root, &question_args)?;
    assert_eq!(limited["limited_coverage"], true);
    assert!(limited["unscanned_records_from"].is_string());
    let fallback = run(&archive, root, &["question", identifier(&question)])?;
    assert_eq!(fallback["records"], overview["records"]);
    assert_eq!(fallback["limited_coverage"], false);

    fs::write(root.join("failed.txt"), "Do not deposit this file")?;
    let before = fs::read(&archive)?;
    assert!(
        !invoke(
            &archive,
            root,
            &[
                "capture",
                "failed.txt",
                "text/plain",
                "--for",
                identifier(&claim)
            ]
        )?
        .status
        .success()
    );
    assert_eq!(fs::read(&archive)?, before);
    Ok(())
}

#[test]
fn capture_keeps_a_public_payloads_link_to_read_only_research_read_only() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let path = root.join("archive.sqlite");
    let archive = Archive::open(&path)?;
    let question = archive
        .document(
            &Document::new(
                Kind::Question,
                "local observation",
                "research",
                json!({}),
                vec![],
            )?,
            Format::A,
            Id::blob(READ_ONLY),
        )?
        .to_string();
    drop(archive);
    fs::write(
        root.join("reading.txt"),
        "Public reading used in a read-only study",
    )?;
    let plain = run(&path, root, &["capture", "reading.txt", "text/plain"])?;
    let public = Id::blob(PUBLIC).to_string();
    let capture_args = [
        "capture",
        "reading.txt",
        "text/plain",
        "--for",
        &question,
        "--access",
        &public,
    ];
    let linked = run(&path, root, &capture_args)?;
    assert_eq!(linked["descriptor"], plain);
    assert_eq!(run(&path, root, &capture_args)?, linked);
    let head = run(&path, root, &["snapshot", "research"])?;
    let show = ["show", identifier(&head), identifier(&linked["occurrence"])];
    assert_eq!(run(&path, root, &show)?["kind"], "occurrence");
    assert!(
        !invoke(
            &path,
            root,
            &[show[0], show[1], show[2], "--purpose", "train"]
        )?
        .status
        .success()
    );
    assert_eq!(
        run(
            &path,
            root,
            &[
                "show",
                identifier(&head),
                identifier(&plain),
                "--purpose",
                "train"
            ]
        )?["kind"],
        "descriptor"
    );
    Ok(())
}

#[test]
fn replay_uses_the_saved_evaluator_and_reassessment_records_changed_standing() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let path: PathBuf = root.join("archive.sqlite");
    let archive = Archive::open(&path)?;
    let unrelated = archive.blob(b"unrelated observation", Id::blob(PUBLIC))?;
    let case = fixture::seed(&archive, Format::A)?;
    fixture::notice(&archive, Format::A, case.calibration, 80, 1)?;
    let corrected = archive.snapshot_id("local review", [case.published].into())?;
    let corrected = corrected.to_string();
    let receipt = case.receipts[2].to_string();
    drop(archive);

    let before = fs::read(&path)?;
    let replay = run(&path, root, &["replay", &receipt, "--head", &corrected])?;
    assert_eq!(replay["matches"], true);
    assert_eq!(replay["head"], json!(case.s0));
    assert_eq!(replay["replayed"]["result"], "supported");
    assert_eq!(fs::read(&path)?, before);

    let changed = run(&path, root, &["reassess", &receipt, "--head", &corrected])?;
    assert_eq!(changed["previous_standing"], "supported");
    assert_eq!(changed["receipt"]["result"], "refuted");
    assert_eq!(changed["head"], corrected);
    assert_ne!(changed["id"], receipt);
    assert_eq!(
        run(&path, root, &["replay", identifier(&changed["id"])])?["matches"],
        true
    );
    assert_eq!(
        run(&path, root, &["reassess", &receipt, "--head", &corrected])?["id"],
        changed["id"]
    );
    let head = run(&path, root, &["snapshot", "local review", "--continue"])?;
    let record = run(
        &path,
        root,
        &["show", identifier(&head), identifier(&changed["id"])],
    )?;
    assert!(
        record["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["role"] == "reassessed-from" && edge["target"] == receipt)
    );
    assert_eq!(
        run(
            &path,
            root,
            &[
                "replay",
                identifier(&changed["id"]),
                "--head",
                identifier(&head)
            ]
        )?["matches"],
        true
    );
    assert_eq!(
        run(
            &path,
            root,
            &["replay", &receipt, "--head", identifier(&head)]
        )?["matches"],
        true
    );

    let original = run(&path, root, &["show", identifier(&head), &receipt])?;
    let mut comparisons = original["fields"]["original_query"].clone();
    comparisons["previous_receipts"] = json!([receipt]);
    fs::write(
        root.join("comparisons.json"),
        serde_json::to_vec(&comparisons)?,
    )?;
    let explicit = run(
        &path,
        root,
        &["assess", identifier(&head), "comparisons.json"],
    )?;
    let mut previous = explicit["id"].clone();
    let mut last_predecessor = previous.clone();
    for _ in 0..5 {
        let active = run(&path, root, &["snapshot", "local review", "--continue"])?;
        let next = run(
            &path,
            root,
            &[
                "reassess",
                identifier(&previous),
                "--head",
                identifier(&active),
            ],
        )?;
        let with_receipt = run(&path, root, &["snapshot", "local review", "--continue"])?;
        let record = run(
            &path,
            root,
            &["show", identifier(&with_receipt), identifier(&next["id"])],
        )?;
        let links = |role: &str| -> Vec<Value> {
            record["inputs"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|edge| edge["role"] == role)
                .map(|edge| edge["target"].clone())
                .collect()
        };
        assert_eq!(links("reassessed-from"), std::slice::from_ref(&previous));
        assert_eq!(links("previous-receipt"), vec![json!(receipt)]);
        assert_eq!(
            run(&path, root, &["replay", identifier(&next["id"])])?["matches"],
            true
        );
        last_predecessor = previous;
        previous = next["id"].clone();
    }

    assert!(
        invoke(&path, root, &["withdraw", &unrelated.to_string()])?
            .status
            .success()
    );
    let after_withdrawal = run(&path, root, &["snapshot", "local review", "--continue"])?;
    assert!(
        !invoke(
            &path,
            root,
            &["replay", &receipt, "--head", identifier(&after_withdrawal)]
        )?
        .status
        .success()
    );
    let default_replay = run(&path, root, &["replay", &receipt])?;
    assert_eq!(default_replay, replay);

    let archive = Archive::open(&path)?;
    let snapshot = archive.head(identifier(&after_withdrawal).parse()?)?;
    let migrated = archive.migrate(&snapshot, Format::B, "local review")?;
    let migrated_receipt = migrated.pairs[&identifier(&previous).parse::<Id>()?];
    assert_eq!(
        archive
            .read_head(migrated.native_head, migrated_receipt, "read")?
            .one("reassessed-from")?,
        identifier(&last_predecessor).parse::<Id>()?
    );
    Ok(())
}
