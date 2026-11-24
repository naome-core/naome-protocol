#![cfg(feature = "cli")]

use anyhow::Result;
use naome_protocol::{
    evaluation::{self, Query, Standing, Status},
    fixture::{self, put},
    profile::{self, Document, Kind, PUBLIC, READ_ONLY, link},
    store::Archive,
    wire::{Format, Id},
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tempfile::tempdir;

#[test]
fn help_uses_nao_with_an_exe_filename() -> Result<()> {
    let directory = tempdir()?;
    let executable = directory.path().join("nao.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_nao"), &executable)?;
    for (args, usage) in [
        (&["--help"][..], "Usage: nao "),
        (&["find", "--help"][..], "Usage: nao find "),
    ] {
        let output = std::process::Command::new(&executable)
            .args(args)
            .output()?;
        assert!(output.status.success());
        let help = String::from_utf8(output.stdout)?;
        assert!(help.lines().any(|line| line.starts_with(usage)), "{help}");
    }
    Ok(())
}

#[test]
fn standalone_commands_leave_the_default_archive_absent() -> Result<()> {
    let directory = tempdir()?;
    let root = directory.path();
    std::fs::write(root.join("kit.json"), include_bytes!("../spec/kits/A.json"))?;
    std::fs::write(
        root.join("model.json"),
        br#"{"losses":[[1]],"joint_masses":[[1]],"loss_unit":"tick","acquisition_cost":0}"#,
    )?;
    let run = |args: &[&str]| -> Result<Vec<u8>> {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_nao"))
            .current_dir(root)
            .args(args)
            .output()?;
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!root.join("evidence.sqlite").exists(), "{args:?}");
        Ok(output.stdout)
    };
    run(&["capabilities"])?;
    run(&["kit", "kit.json", &naome_protocol::kit::pin(Format::A)])?;
    run(&["keygen", "seed"])?;
    run(&["value", "model.json"])?;
    run(&["trial", "trial"])?;
    let submission = run(&["suite", "trial/conformance"])?;
    std::fs::write(root.join("submission.json"), submission)?;
    run(&["grade", "trial/conformance/answers.json", "submission.json"])?;

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nao"))
        .current_dir(root)
        .arg("init")
        .output()?;
    assert!(output.status.success());
    assert!(root.join("evidence.sqlite").is_file());
    Ok(())
}

#[test]
fn capabilities_advertise_the_receipt_writer_and_replay_ids() -> Result<()> {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nao"))
        .arg("capabilities")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let capabilities: Value = serde_json::from_slice(&output.stdout)?;
    let archive = Archive::memory()?;
    let case = fixture::seed(&archive, Format::A)?;
    let receipt = archive.read(&archive.head(case.published)?, case.receipts[2], "read")?;
    assert_eq!(receipt.fields["evaluator"], "rep-rust-rules-1");
    let replay =
        serde_json::from_value::<BTreeSet<String>>(capabilities["replay_evaluators"].clone())?;
    for (field, capability) in [
        (None, "receipt_evaluator"),
        (
            Some("foreign_calibration_keys"),
            "calibration_identity_evaluator",
        ),
        (Some("input_correction_keys"), "input_correction_evaluator"),
    ] {
        let mut query = case.queries[2].clone();
        let seed = archive.head(case.published)?;
        let mut policy = archive.read(&seed, query.policy, "read")?;
        if let Some(field) = field {
            policy.fields.insert(field.into(), json!({}));
        }
        query.policy = archive.document(&policy, Format::A, Id::blob(PUBLIC))?;
        let head = archive.snapshot("correction policy", [case.published].into())?;
        let result = evaluation::assess(&archive, &head, &query)?;
        let id = evaluation::deposit_receipt(&archive, &head, &query, &result, Format::A)?;
        let after = archive.snapshot("correction receipt", [head.id()?].into())?;
        let stored = archive.read(&after, id, "read")?;
        assert_eq!(capabilities[capability], stored.fields["evaluator"]);
        assert_eq!(capabilities[capability], "rep-rust-rules-6");
    }
    let expected = [
        "rep-rust-rules-6",
        "rep-rust-rules-5",
        "rep-rust-rules-4",
        "rep-rust-rules-3",
        "rep-rust-rules-2",
        "rep-rust-rules-1",
        "rep-rust-0.1.2",
        "rep-rust-0.1.2-dev.1",
        "rep-rust-0.1.2-dev",
        "rep-rust-0.1.1",
        "rep-rust-0.1.0",
    ]
    .map(String::from)
    .into();
    assert_eq!(replay, expected);
    Ok(())
}

#[test]
fn projection_uses_only_the_selected_receipt_horizon() -> Result<()> {
    for format in [Format::A, Format::B] {
        let archive = Archive::memory()?;
        let case = fixture::seed(&archive, format)?;
        let full = archive.head(case.published)?;
        let receipt = case.receipts[0];
        let original = archive.read(&full, receipt, "read")?;
        let snapshot_id = original.one("snapshot")?;
        let historical = archive.head(snapshot_id)?;
        let project = |view: &naome_protocol::store::Snapshot| {
            archive.project_hull(view, receipt, &fixture::fixture_key(3), format)
        };
        project(&full)?;
        let mut members = full.objects.clone();
        members.remove(&snapshot_id);
        let without_manifest = archive.seal_members(&full.source, full.parents.clone(), members)?;
        assert!(project(&without_manifest).is_err());
        let mut members = full.objects.clone();
        assert!(historical.objects.contains(&case.policy));
        members.remove(&case.policy);
        let without_policy = archive.seal_members(&full.source, full.parents.clone(), members)?;
        assert!(project(&without_policy).is_err());
    }
    Ok(())
}

#[test]
fn full_exchange_is_reproducible_from_reopened_databases() -> Result<()> {
    let dir = tempdir()?;
    let results = fixture::trial(dir.path())?;
    let west = Archive::open(dir.path().join("west.sqlite"))?;
    let s1 = west.head(serde_json::from_value(results["S1"].clone())?)?;
    let qs: Vec<Query> = serde_json::from_value(results["native_queries"].clone())?;
    let r = evaluation::assess(&west, &s1, &qs[2])?;
    assert_eq!(r.result, Some(Standing::Refuted));
    assert_eq!(r.branches[0]["joint"], json!([150, 200]));
    let old = west.head(serde_json::from_value(results["fixture"]["s0"].clone())?)?;
    let original: Vec<Query> = serde_json::from_value(results["fixture"]["queries"].clone())?;
    assert_eq!(
        evaluation::assess(&west, &old, &original[2])?.result,
        Some(Standing::Supported)
    );
    let id: Id = serde_json::from_value(results["fixture"]["receipts"][2].clone())?;
    let mapping: Id = serde_json::from_value(results["mapping"].clone())?;
    let pairs = west.verify_mapping(&s1, mapping)?;
    let sig = west
        .discover(
            &s1,
            Some(Kind::Attestation),
            None,
            None,
            "read",
            10000,
            None,
        )?
        .ids
        .into_iter()
        .filter_map(|i| west.read(&s1, i, "read").ok())
        .find(|d| d.one("signed-original").ok() == Some(id))
        .expect("original signature");
    let trusted = [hex::encode(
        fixture::fixture_key(4).verifying_key().to_bytes(),
    )]
    .into();
    profile::verify_attestation(
        &sig,
        id,
        "transition-1",
        "assessor",
        "research-policy-1",
        &trusted,
    )?;
    assert!(
        profile::verify_attestation(
            &sig,
            pairs[&id],
            "transition-1",
            "assessor",
            "research-policy-1",
            &trusted
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn measurement_uses_the_declared_guarantee_unit() -> Result<()> {
    let archive = Archive::memory()?;
    let public = Id::blob(PUBLIC);
    let case = fixture::seed(&archive, Format::A)?;
    let original = archive.head(case.s0)?;
    let mut query = case.queries[2].clone();
    let mut argument = archive.read(&original, query.arguments[0], "read")?;
    let mut guarantee = archive.read(&original, argument.one("guarantee")?, "read")?;
    for (unit, joint, standing) in [
        ("um", Some([230, 280]), Standing::Supported),
        ("mm", Some([-29740, 30250]), Standing::Unresolved),
        ("unrecognised", None, Standing::Unresolved),
    ] {
        guarantee.fields.insert("unit".into(), json!(unit));
        let id = archive.document(&guarantee, Format::A, public)?;
        argument
            .inputs
            .iter_mut()
            .find(|e| e.role == "guarantee")
            .unwrap()
            .target = id;
        query.arguments = vec![archive.document(&argument, Format::A, public)?];
        let inventory = archive.snapshot("measurement", [case.s0].into())?;
        let receipt = evaluation::assess(&archive, &inventory, &query)?;
        assert_eq!(receipt.result, Some(standing), "{unit}");
        if let Some(bounds) = joint {
            assert_eq!(receipt.branches[0]["joint"], json!(bounds));
        } else {
            assert!(receipt.branches.is_empty());
            assert!(receipt.obligations.iter().any(|s| s.contains("unit")));
        }
    }
    Ok(())
}

#[test]
fn partition_rivals_scope_unknown_and_budget() -> Result<()> {
    let a = Archive::memory()?;
    let i = fixture::seed(&a, Format::A)?;
    let initial = a.head(i.s0)?;
    let mut q = i.queries[2].clone();
    q.current_at = Some(100);
    assert_eq!(
        evaluation::assess(&a, &initial, &q)?.result,
        Some(Standing::Unresolved)
    );
    q.current_at = None;
    q.budget = 1;
    assert!(!evaluation::assess(&a, &initial, &q)?.frontier.is_empty());
    q.budget = 10000;
    fixture::notice(&a, Format::A, i.calibration, 10, 2)?;
    let only_m = a.snapshot("east", [i.s0].into())?;
    assert_eq!(
        evaluation::assess(&a, &only_m, &q)?.branches[0]["joint"],
        json!([220, 270])
    );
    let n = fixture::notice(&a, Format::A, i.calibration, 80, 1)?;
    let both = a.snapshot("east", [only_m.id()?].into())?;
    let r = evaluation::assess(&a, &both, &q)?;
    assert_eq!(r.result, Some(Standing::Contested));
    assert_eq!(r.applicable_notices.len(), 2);
    assert!(!r.obligations.is_empty());
    let impact = a.impact(&both, n, &[], both.objects.len())?;
    assert_eq!(impact.by_kind["label"].len(), 4);
    assert!(
        i.labels
            .iter()
            .all(|id| impact.by_kind["label"].contains(id))
    );
    assert!(
        !impact
            .by_kind
            .values()
            .any(|v| v.contains(&i.control) || v.contains(&i.control_label))
    );
    Ok(())
}

fn argument_case(
    method: &str,
    claim: Value,
    inputs: impl FnOnce(&Archive) -> Result<Vec<profile::Link>>,
    fields: Value,
) -> Result<evaluation::Receipt> {
    let a = Archive::memory()?;
    let f = Format::B;
    let policy = fixture::policy(&a, f)?;
    let c = put(&a, f, Kind::Claim, "subject", "scope", claim, vec![])?;
    let mut links = inputs(&a)?;
    links.push(link("claim", c));
    let mut body = fields;
    body["method"] = json!(method);
    let arg = put(&a, f, Kind::Argument, "subject", "scope", body, links)?;
    evaluation::assess(
        &a,
        &a.snapshot("case", BTreeSet::new())?,
        &Query {
            target: c,
            arguments: vec![arg],
            policy,
            budget: 1000,
            byte_limit: 64 * 1024 * 1024,
            mappings: vec![],
            previous_receipts: vec![],
            required_notices: vec![],
            current_at: None,
        },
    )
}

#[test]
fn unit_mapping_requires_definitions_and_distinguishing_evidence() -> Result<()> {
    for (functional, domain, known, expected) in [
        ("endpoint-change", 1000, true, Standing::Supported),
        ("endpoint-change", 200, true, Standing::Refuted),
        ("peak-to-peak", 1000, true, Standing::Refuted),
        ("endpoint-change", 1000, false, Standing::Unresolved),
    ] {
        let r = argument_case(
            "unit-grid-map",
            json!({"predicate":"exact-translation"}),
            |a| {
                let mut inputs = vec![];
                for (role, unit, m, max) in [
                    ("source-profile", "um", "endpoint-change", 1000),
                    ("target-profile", "mm", functional, domain),
                ] {
                    let mut fields = json!({"unit":unit,"functional":m,"max_um":max,"step_um":1});
                    if !known {
                        fields.as_object_mut().unwrap().remove("functional");
                    }
                    inputs.push(link(
                        role,
                        put(
                            a,
                            Format::B,
                            Kind::Profile,
                            "subject",
                            "scope",
                            fields,
                            vec![],
                        )?,
                    ));
                }
                for trace in [vec![0, 0], vec![0, 200], vec![0, 250], vec![0, 260, 180]] {
                    inputs.push(link(
                        "trace",
                        put(
                            a,
                            Format::B,
                            Kind::Occurrence,
                            "subject",
                            "scope",
                            json!({"positions_um":trace}),
                            vec![],
                        )?,
                    ));
                }
                Ok(inputs)
            },
            json!({}),
        )?;
        assert_eq!(r.result, Some(expected));
        if known && functional == "peak-to-peak" {
            assert_eq!(r.branches[1]["witness"]["target_um"], 260);
            assert_eq!(r.branches[1]["witness"]["source_um"], 180);
        }
    }
    Ok(())
}

#[test]
fn restricted_proof_does_not_promote_the_theorem() -> Result<()> {
    let r = argument_case(
        "least-zero",
        json!({"predicate":"least-element","domain":"finite-nat-sets-containing-zero"}),
        |a| {
            Ok(vec![link(
                "proof",
                put(
                    a,
                    Format::B,
                    Kind::Derivation,
                    "subject",
                    "scope",
                    json!({"kernel":"nat-order-zero-v1","steps":["member-zero","zero-le-every-nat","least-intro"]}),
                    vec![],
                )?,
            )])
        },
        json!({}),
    )?;
    assert_eq!(r.result, Some(Standing::Supported));
    let coverage = argument_case(
        "domain-inclusion",
        json!({"predicate":"covers-domain","domain":"nonempty-finite-nat-sets"}),
        |a| {
            Ok(vec![link(
                "restricted",
                put(
                    a,
                    Format::B,
                    Kind::Claim,
                    "subject",
                    "scope",
                    json!({"domain":"finite-nat-sets-containing-zero"}),
                    vec![],
                )?,
            )])
        },
        json!({"witness":[3]}),
    )?;
    assert_eq!(coverage.result, Some(Standing::Refuted));
    assert_eq!(coverage.branches[0]["counterexample"], json!([3]));
    Ok(())
}

#[test]
fn manuscript_and_survey_evidence_observes_shared_origins() -> Result<()> {
    let r = argument_case(
        "source-independence",
        json!({"predicate":"independent-witnesses"}),
        |a| {
            let m = put(
                a,
                Format::B,
                Kind::Occurrence,
                "subject",
                "scope",
                json!({"edition":"first"}),
                vec![],
            )?;
            (0..2)
                .map(|n| {
                    Ok(link(
                        "transcription",
                        put(
                            a,
                            Format::B,
                            Kind::Derivation,
                            "subject",
                            "scope",
                            json!({"edition":n}),
                            vec![link("manuscript", m)],
                        )?,
                    ))
                })
                .collect()
        },
        json!({}),
    )?;
    assert_eq!(r.result, Some(Standing::Refuted));
    for (method, pred, want) in [
        ("sample-description", "sample-reports", Standing::Supported),
        (
            "survey-identification",
            "population-effect",
            Standing::Unresolved,
        ),
    ] {
        let r = argument_case(
            method,
            json!({"predicate":pred,"count":2}),
            |a| {
                ["alice", "bob", "bob"]
                    .into_iter()
                    .enumerate()
                    .map(|(copy, unit)| {
                        Ok(link(
                            "response",
                            put(
                                a,
                                Format::B,
                                Kind::Occurrence,
                                "subject",
                                "scope",
                                json!({"population":"subject","respondent":unit,"copy":copy,"sampling_frame":"route-riders"}),
                                vec![],
                            )?,
                        ))
                    })
                    .collect()
            },
            json!({}),
        )?;
        assert_eq!(r.result, Some(want));
        assert_eq!(r.branches[0]["unique_reported_units"], 2);
    }
    Ok(())
}

#[test]
fn controlled_test_exemptions_require_witness_and_a_scientific_pass() -> Result<()> {
    let a = Archive::memory()?;
    let f = Format::A;
    let public = Id::blob(PUBLIC);
    let key = hex::encode(fixture::fixture_key(22).verifying_key().to_bytes());
    let policy = put(
        &a,
        f,
        Kind::Policy,
        "board",
        "research-policy-1",
        json!({"keys":{"reviewer":[key]},"rules":{"attribution":{"predicate":"attributed","polarity":"pro","required":["attribution","read-content"],"scientific":["attribution"],"exemptions":{"read-content":"sealed-custody"},"role":"reviewer"}}}),
        vec![],
    )?;
    let claim = put(
        &a,
        f,
        Kind::Claim,
        "testimony",
        "edition",
        json!({"predicate":"attributed"}),
        vec![],
    )?;
    let mk = |name: &str, status: Status| -> Result<Id> {
        let id = put(
            &a,
            f,
            Kind::Assessment,
            "testimony",
            "edition",
            json!({"rule":"attribution","test":name,"status":status}),
            vec![link("claim", claim)],
        )?;
        fixture::sign(&a, f, id, "edition", "reviewer", 22)?;
        Ok(id)
    };
    let attribution = mk("attribution", Status::Pass)?;
    let sealed = mk("read-content", Status::NotApplicable)?;
    let evidence = mk("sealed-custody", Status::Pass)?;
    let mut links = vec![
        link("claim", claim),
        link("test", attribution),
        link("test", sealed),
    ];
    let partial = put(
        &a,
        f,
        Kind::Argument,
        "testimony",
        "edition",
        json!({"rule":"attribution","method":"signed-tests"}),
        links.clone(),
    )?;
    links.push(link("test", evidence));
    let complete = put(
        &a,
        f,
        Kind::Argument,
        "testimony",
        "edition",
        json!({"rule":"attribution","method":"signed-tests"}),
        links,
    )?;
    let secret = a.blob(b"confidential testimony", Id::blob(READ_ONLY))?;
    let d = Document::new(
        Kind::Derivation,
        "testimony",
        "edition",
        json!({"method":"sealed transcription"}),
        vec![link("source", secret)],
    )?;
    let transcript = a.document(&d, f, public)?;
    let s = a.snapshot("custodian", BTreeSet::new())?;
    let mut q = Query {
        target: claim,
        arguments: vec![partial],
        policy,
        budget: 1000,
        byte_limit: 1000000,
        mappings: vec![],
        previous_receipts: vec![],
        required_notices: vec![],
        current_at: None,
    };
    assert_eq!(
        evaluation::assess(&a, &s, &q)?.result,
        Some(Standing::Unresolved)
    );
    q.arguments.push(complete);
    assert_eq!(
        evaluation::assess(&a, &s, &q)?.result,
        Some(Standing::Supported)
    );
    assert!(a.export(&s, "train")?.omitted.contains(&transcript));
    q.byte_limit = 8;
    assert!(!evaluation::assess(&a, &s, &q)?.frontier.is_empty());
    Ok(())
}

#[test]
fn trial_rejects_an_existing_conformance_directory_before_creating_archives() -> Result<()> {
    let directory = tempdir()?;
    let cases = directory.path().join("conformance");
    std::fs::create_dir(&cases)?;
    let marker = cases.join("existing.json");
    std::fs::write(&marker, b"existing result")?;

    let trial = std::process::Command::new(env!("CARGO_BIN_EXE_nao"))
        .arg("trial")
        .arg(directory.path())
        .output()?;
    assert!(!trial.status.success());
    let error = String::from_utf8(trial.stderr)?;
    assert!(error.contains("fresh conformance directory"), "{error}");
    assert_eq!(std::fs::read(&marker)?, b"existing result");
    let remaining = std::fs::read_dir(directory.path())?.collect::<std::io::Result<Vec<_>>>()?;
    assert_eq!(remaining.len(), 1);
    Ok(())
}

#[test]
fn public_interchange_runs_through_cli_and_grades_external_output() -> Result<()> {
    use std::process::Command;
    let dir = tempdir()?;
    let nao = env!("CARGO_BIN_EXE_nao");
    let trial = Command::new(nao).arg("trial").arg(dir.path()).output()?;
    assert!(
        trial.status.success(),
        "{}",
        String::from_utf8_lossy(&trial.stderr)
    );
    let cases = dir.path().join("conformance");
    let run = Command::new(nao).arg("suite").arg(&cases).output()?;
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let submission = dir.path().join("actual.json");
    std::fs::write(&submission, &run.stdout)?;
    let grade = || {
        Command::new(nao)
            .arg("grade")
            .arg(cases.join("answers.json"))
            .arg(&submission)
            .output()
    };
    let good = grade()?;
    assert!(good.status.success());
    let score: Value = serde_json::from_slice(&good.stdout)?;
    assert_eq!(score["correct"], 24);
    assert_eq!(score["incorrect_acceptance"], 0);
    let mut wrong: Value = serde_json::from_slice(&run.stdout)?;
    wrong["restricted/above"] = json!("supported");
    std::fs::write(&submission, serde_json::to_vec(&wrong)?)?;
    let bad = grade()?;
    assert!(!bad.status.success());
    let score: Value = serde_json::from_slice(&bad.stdout)?;
    assert_eq!(score["incorrect_acceptance"], 1);

    let cases_path = cases.join("cases.json");
    let original: Value = serde_json::from_slice(&std::fs::read(&cases_path)?)?;
    let full = &original["corrected/score-above"];
    let mut record_limited = full.clone();
    record_limited["query"]["budget"] = json!(1);
    let mut byte_limited = full.clone();
    byte_limited["query"]["byte_limit"] = json!(1);
    let mixed = json!({
        "a-record-limit": record_limited,
        "b-full": full,
        "c-byte-limit": byte_limited,
    });
    std::fs::write(cases_path, serde_json::to_vec(&mixed)?)?;
    let run = Command::new(nao).arg("suite").arg(cases).output()?;
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&run.stdout)?,
        json!({"a-record-limit": "unknown", "b-full": "fail", "c-byte-limit": "unknown"})
    );
    Ok(())
}

#[test]
fn withdrawals_leave_historic_proofs_and_unaffected_arguments_available() -> Result<()> {
    let a = Archive::memory()?;
    let f = Format::A;
    let make = |k, b, e| put(&a, f, k, "theorem", "scope", b, e);
    let key = hex::encode(fixture::fixture_key(9).verifying_key().to_bytes());
    let policy = put(
        &a,
        f,
        Kind::Policy,
        "custodian",
        "research-policy-1",
        json!({"keys":{"notice":[key]}}),
        vec![],
    )?;
    let c = make(
        Kind::Claim,
        json!({"predicate":"least-element","domain":"finite-nat-sets-containing-zero"}),
        vec![],
    )?;
    let mut arguments = vec![];
    for version in [1, 2] {
        let proof = make(
            Kind::Derivation,
            json!({"kernel":"nat-order-zero-v1","steps":["member-zero","zero-le-every-nat","least-intro"],"edition":version}),
            vec![],
        )?;
        arguments.push(make(
            Kind::Argument,
            json!({"method":"least-zero"}),
            vec![link("claim", c), link("proof", proof)],
        )?);
    }
    let initial = a.snapshot("before", BTreeSet::new())?;
    let mut query = Query {
        target: c,
        policy,
        arguments: vec![arguments[0]],
        budget: 1000,
        byte_limit: 1000000,
        mappings: vec![],
        previous_receipts: vec![],
        required_notices: vec![],
        current_at: None,
    };
    let n = make(
        Kind::Notice,
        json!({"reason":"editor withdrew this edition","action":"withdraw"}),
        vec![link("target", arguments[0])],
    )?;
    fixture::sign(&a, f, n, "scope", "notice", 9)?;
    let current = a.snapshot("after", [initial.id()?].into())?;
    assert_eq!(
        evaluation::assess(&a, &initial, &query)?.result,
        Some(Standing::Supported)
    );
    assert_eq!(
        evaluation::assess(&a, &current, &query)?.result,
        Some(Standing::Unresolved)
    );
    // Deliver every page except the signed notice's page, keeping the same head.
    let mut mirror = Archive::memory()?;
    let mut after = None;
    let mut notice_page = None;
    loop {
        let page = a.export_page(&current, "read", after, 1)?;
        after = page.continuation;
        if page.packets.iter().any(|packet| packet.id == n) {
            notice_page = Some(page);
        } else {
            mirror.import(&page)?;
        }
        if after.is_none() {
            break;
        }
    }
    let imported = mirror.head(current.id()?)?;
    let partial = evaluation::assess(&mirror, &imported, &query)?;
    assert!(!partial.horizon_complete);
    assert_eq!(partial.frontier, [n].into());
    assert_eq!(partial.result, Some(Standing::Unresolved));
    mirror.import(&notice_page.expect("exported notice"))?;
    let complete = evaluation::assess(&mirror, &imported, &query)?;
    assert!(complete.horizon_complete && complete.frontier.is_empty());
    assert_eq!(complete.result, Some(Standing::Unresolved));
    assert!(complete.applicable_notices.contains(&n));

    query.arguments = arguments;
    assert_eq!(
        evaluation::assess(&a, &current, &query)?.result,
        Some(Standing::Supported)
    );
    Ok(())
}

#[test]
fn legacy_calibration_fixture_retains_its_published_identity() -> Result<()> {
    // Original manifest, receipt, and access descriptor from the writer at 78419e5.
    let bundle = naome_protocol::store::Bundle::read(
        &include_bytes!("fixtures/legacy-calibration-receipt.json")[..],
    )?;
    let mut archive = Archive::memory()?;
    let active = archive.import(&bundle)?;
    let original: Id =
        "1:1:b7826ed27f6f1fdc30fad29e48727cc2fef11b9c7316669f3fef7018700ef0a9".parse()?;
    let receipt: Id =
        "1:0:ee829956a6e4e9a2469d51ed8385c85b493ca9f286c6ea132d85fa05c16bded4".parse()?;
    assert_eq!(bundle.head, original);
    assert_eq!(archive.head(original)?.id()?, original);
    assert_ne!(active, original);
    for head in [original, active] {
        receipt.verify(&archive.resolve(&archive.head(head)?, receipt, "read")?)?;
    }
    Ok(())
}
