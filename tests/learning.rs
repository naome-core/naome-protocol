#![cfg(feature = "cli")]

use anyhow::Result;
use naome_protocol::{
    evaluation::{self, Query, Standing},
    fixture::{self, put},
    learning::{Example, TrainingRequest},
    profile::{Kind, PUBLIC, READ_ONLY, link},
    store::Archive,
    wire::{Format, Id},
};
use serde_json::json;
use std::collections::BTreeSet;

#[test]
fn learning_cutoff_permissions_independence_and_false_labels() -> Result<()> {
    let a = Archive::memory()?;
    let f = Format::A;
    let i = fixture::seed(&a, f)?;
    let s = a.head(i.published)?;
    let plan = a.read(&s, i.queries[0].arguments[0], "read")?.one("plan")?;
    let req = TrainingRequest {
        feature_cutoff: i.feature_cutoff,
        forecast_plan: Some(plan),
        mappings: vec![],
        label_head: i.published,
        policy: i.policy,
        standing: Default::default(),
        project: None,
        examples: vec![
            Example {
                features: vec![plan],
                label: i.labels[0],
                partition: "train".into(),
            },
            Example {
                features: vec![plan],
                label: i.labels[1],
                partition: "test".into(),
            },
            Example {
                features: vec![i.control],
                label: i.labels[2],
                partition: "train".into(),
            },
        ],
        independent_roles: ["calibration".into()].into(),
        binary: true,
    };
    let m = a.training_manifest(&req, f)?;
    assert_eq!(m.included, vec![i.labels[0]]);
    assert_eq!(m.withheld.len(), 2);
    let mut d = a.read(&s, i.labels[0], "read")?;
    d.fields.insert("value".into(), json!("fail"));
    let wrong = a.document(&d, f, Id::blob(PUBLIC))?;
    let sealed = a.document(&a.read(&s, plan, "read")?, f, Id::blob(READ_ONLY))?;
    let head = a.snapshot("later", [s.id()?].into())?.id()?;
    let req = TrainingRequest {
        feature_cutoff: head,
        label_head: head,
        examples: vec![
            Example {
                features: vec![plan],
                label: wrong,
                partition: "train".into(),
            },
            Example {
                features: vec![sealed],
                label: i.labels[3],
                partition: "train".into(),
            },
        ],
        ..req
    };
    assert!(a.training_manifest(&req, f)?.included.is_empty());
    assert!(a.export(&a.head(head)?, "train")?.omitted.contains(&sealed));
    Ok(())
}

#[test]
fn prior_release_receipts_must_replay_before_their_labels_can_be_used() -> Result<()> {
    let archive = Archive::memory()?;
    let format = Format::A;
    let case = fixture::seed(&archive, format)?;
    let evidence = archive.head(case.published)?;
    let original = archive.read(&evidence, case.receipts[2], "read")?;
    assert_eq!(original.fields["evaluator"], json!("rep-rust-rules-1"));
    let plan = archive
        .read(&evidence, case.queries[2].arguments[0], "read")?
        .one("plan")?;
    for (evaluator, alter_transcript, accepted) in [
        ("rep-rust-0.1.0", false, true),
        ("rep-rust-0.1.0", true, false),
        ("rep-rust-0.1.1", false, true),
        ("rep-rust-0.1.1", true, false),
        ("rep-rust-0.1.2-dev", false, true),
        ("rep-rust-0.1.2-dev", true, false),
        ("rep-rust-0.1.2-dev.1", false, true),
        ("rep-rust-0.1.2-dev.1", true, false),
        ("rep-rust-0.1.2", false, true),
        ("rep-rust-0.1.2", true, false),
        ("unknown-evaluator", false, false),
    ] {
        let mut receipt = original.clone();
        receipt.fields.insert("evaluator".into(), json!(evaluator));
        if alter_transcript {
            receipt.fields.get_mut("original_transcript").unwrap()["horizon_complete"] =
                json!(false);
        }
        let id = archive.document(&receipt, format, Id::blob(PUBLIC))?;
        let receipts = archive.snapshot("received assessments", [case.published].into())?;
        let label = archive.label(&receipts, id, "synthetic", None, None, format)?;
        let labels = archive.snapshot("received labels", [receipts.id()?].into())?;
        let request = TrainingRequest {
            policy: case.policy,
            feature_cutoff: case.feature_cutoff,
            label_head: labels.id()?,
            examples: vec![Example {
                features: vec![plan],
                label,
                partition: "train".into(),
            }],
            standing: naome_protocol::learning::LabelStanding::Historical,
            forecast_plan: None,
            mappings: vec![],
            project: None,
            independent_roles: BTreeSet::new(),
            binary: true,
        };
        let manifest = archive.training_manifest(&request, format)?;
        assert_eq!(
            manifest.included.contains(&label),
            accepted,
            "{:?}",
            manifest.withheld
        );
        assert_eq!(manifest.withheld.contains_key(&label), !accepted);
    }
    Ok(())
}

#[test]
fn released_v012_receipt_replays_with_its_original_withdrawal_semantics() -> Result<()> {
    // Exported by tag v0.1.2 (c546fba6ed08403c045b3438fd7ad446de258c5f).
    // The query requires an axial withdrawal, and the reading is radial.
    #[derive(serde::Deserialize)]
    struct Vector {
        bundle: naome_protocol::store::Bundle,
        receipt: Id,
        request: TrainingRequest,
    }

    let mut vector: Vector =
        serde_json::from_str(include_str!("fixtures/rep-v0.1.2-required-withdrawal.json"))?;
    let mut archive = Archive::memory()?;
    let head = archive.import(&vector.bundle)?;
    assert_eq!(head, vector.request.label_head);
    let label = vector.request.examples[0].label;
    let view = archive.head(head)?;
    let receipt = archive.read(&view, vector.receipt, "read")?;
    assert_eq!(receipt.fields["evaluator"], json!("rep-rust-0.1.2"));
    let original: evaluation::Receipt = receipt.field("original_transcript")?;
    assert_eq!(original.result, Some(Standing::Unresolved));
    assert!(!original.horizon_complete);
    assert!(
        original
            .obligations
            .contains("required notice applicability unknown")
    );

    let replay = archive.training_manifest(&vector.request, Format::A)?;
    assert_eq!(replay.included, [label]);
    assert!(replay.withheld.is_empty());

    let mut repeated = vector.request.clone();
    let example = repeated.examples[0].clone();
    repeated.examples.push(example.clone());
    repeated.examples.push(Example {
        features: vec![],
        ..example
    });
    let rows = archive.training_manifest(&repeated, Format::A)?;
    assert_eq!(rows.included, [label]);
    assert_eq!(
        rows.rows
            .iter()
            .map(|row| (row.label, row.rejection.as_deref()))
            .collect::<Vec<_>>(),
        [
            (label, None),
            (label, Some("duplicate training example")),
            (label, Some("empty features or partition")),
        ]
    );

    let horizon = archive.head(receipt.one("snapshot")?)?;
    let query: Query = receipt.field("original_query")?;
    let current = evaluation::assess(&archive, &horizon, &query)?;
    assert!(current.horizon_complete);
    assert_eq!(current.result, Some(Standing::Supported));
    vector.request.standing = naome_protocol::learning::LabelStanding::Current;
    let extraction = archive.training_manifest(&vector.request, Format::A)?;
    assert!(extraction.included.is_empty());
    assert_eq!(
        extraction.withheld[&label],
        "label requires reassessment at the requested notice horizon"
    );
    Ok(())
}

#[test]
fn training_reuses_inventories_without_widening_query_budgets() -> Result<()> {
    let archive = Archive::memory()?;
    let format = Format::A;
    let case = fixture::seed(&archive, format)?;
    let evidence = archive.head(case.published)?;
    let query = &case.queries[2];
    let plan = archive
        .read(&evidence, query.arguments[0], "read")?
        .one("plan")?;
    let record_bytes = evidence
        .objects
        .iter()
        .filter(|id| id.format().is_some())
        .try_fold(0, |total, &id| -> Result<usize> {
            Ok(total + archive.resolve(&evidence, id, "read")?.len())
        })?;
    let queries = [
        query.clone(),
        Query {
            budget: evidence.objects.len(),
            ..query.clone()
        },
        Query {
            byte_limit: record_bytes,
            ..query.clone()
        },
    ];
    let receipts = queries
        .iter()
        .map(|query| -> Result<Id> {
            let receipt = evaluation::assess(&archive, &evidence, query)?;
            assert!(receipt.horizon_complete);
            assert_eq!(receipt.result, Some(Standing::Supported));
            evaluation::deposit_receipt(&archive, &evidence, query, &receipt, format)
        })
        .collect::<Result<Vec<_>>>()?;
    let assessed = archive.snapshot("assessed", [case.published].into())?;
    let labels = receipts
        .into_iter()
        .map(|id| archive.label(&assessed, id, "synthetic", None, None, format))
        .collect::<Result<Vec<_>>>()?;
    let labeled = archive.snapshot("labeled", [assessed.id()?].into())?;
    let mut request = TrainingRequest {
        feature_cutoff: case.feature_cutoff,
        forecast_plan: None,
        mappings: vec![],
        label_head: labeled.id()?,
        policy: case.policy,
        standing: Default::default(),
        project: None,
        examples: labels
            .iter()
            .map(|&label| Example {
                features: vec![plan],
                label,
                partition: "train".into(),
            })
            .collect(),
        independent_roles: BTreeSet::new(),
        binary: true,
    };
    let current = archive.training_manifest(&request, format)?;
    assert_eq!(current.included, labels[..1]);
    for label in &labels[1..] {
        assert_eq!(
            current.withheld[label],
            "label requires reassessment at the requested notice horizon"
        );
    }
    request.standing = naome_protocol::learning::LabelStanding::Historical;
    assert_eq!(
        archive.training_manifest(&request, format)?.included,
        labels
    );
    Ok(())
}

#[test]
fn adoption_compares_frozen_arms_and_rejects_exposure() -> Result<()> {
    use naome_protocol::adoption::Adoption;
    let a = Archive::memory()?;
    let f = Format::B;
    let public = Id::blob(PUBLIC);
    let put = |k, body, edges| fixture::put(&a, f, k, "successor", "held-out trial", body, edges);
    let task = put(Kind::Question, json!({"question":"finite task"}), vec![])?;
    let protected = put(
        Kind::Manifest,
        json!({"distribution":"one task"}),
        vec![link("task", task)],
    )?;
    let training = put(Kind::Manifest, json!({"distribution":"other data"}), vec![])?;
    let parent = put(Kind::Model, json!({"controller":"parent owner"}), vec![])?;
    let candidate = put(
        Kind::Model,
        json!({"controller":"candidate owner"}),
        vec![link("parent", parent), link("training", training)],
    )?;
    let plan = put(
        Kind::Plan,
        json!({"rule":"paired-count-v1","per_arm_budget":{"steps":100},"minimum_gain":1,"use_range":"held-out family","reassess_when":"correction or regression"}),
        vec![
            link("parent", parent),
            link("candidate", candidate),
            link("protected", protected),
        ],
    )?;
    let sealed = a.snapshot("precommit", BTreeSet::new())?;
    let sealed_head = a.blob(&serde_json::to_vec(&sealed)?, public)?;
    for exposures in [0, 1] {
        let observation = put(
            Kind::Occurrence,
            json!({"concealed_until_test":true,"previous_exposures":exposures,"candidate_budget":{"steps":100},"parent_budget":{"steps":100},"completed_per_arm":1}),
            vec![
                link("parent", parent),
                link("candidate", candidate),
                link("protected", protected),
                link("plan", plan),
                link("sealed-head", sealed_head),
                link("candidate-success", task),
            ],
        )?;
        let sig = fixture::sign(&a, f, observation, "held-out trial", "trial-controller", 21)?;
        let head = a.snapshot("trial", [sealed_head].into())?;
        let request = Adoption {
            plan,
            sealed_head,
            observation,
            signature: sig,
        };
        let result = a.adoption(
            &head,
            &request,
            &[hex::encode(
                fixture::fixture_key(21).verifying_key().to_bytes(),
            )]
            .into(),
            f,
        );
        if exposures == 0 {
            let id = result?;
            let outcome = a.read(&a.snapshot("decision", [head.id()?].into())?, id, "read")?;
            assert!(outcome.field::<bool>("adopted")?);
            assert_eq!(outcome.one("rollback")?, parent);
        } else {
            assert!(result.is_err());
        }
    }
    Ok(())
}

#[test]
fn training_rejects_duplicate_targets_false_origins_and_superseded_labels() -> Result<()> {
    use naome_protocol::learning::LabelStanding;
    let a = Archive::memory()?;
    let f = Format::A;
    let i = fixture::seed(&a, f)?;
    let original = a.head(i.published)?;
    let plan = a
        .read(&original, i.queries[2].arguments[0], "read")?
        .one("plan")?;
    assert!(
        a.label(&original, i.receipts[2], "observed", None, None, f)
            .is_err()
    );

    let mut req = TrainingRequest {
        feature_cutoff: i.feature_cutoff,
        label_head: i.published,
        policy: i.policy,
        standing: LabelStanding::Current,
        forecast_plan: Some(plan),
        mappings: vec![],
        project: None,
        examples: ["train", "test"]
            .iter()
            .map(|part| Example {
                features: vec![plan],
                label: i.labels[2],
                partition: (*part).into(),
            })
            .collect(),
        independent_roles: BTreeSet::new(),
        binary: true,
    };
    let split = a.training_manifest(&req, f)?;
    assert_eq!(split.included, vec![i.labels[2]]);
    assert!(split.withheld[&i.labels[2]].contains("crosses declared partitions"));

    let mut incorrect = a.read(&original, i.labels[2], "read")?;
    incorrect.fields.insert("origin".into(), json!("observed"));
    let incorrect_id = a.document(&incorrect, f, Id::blob(PUBLIC))?;
    req.label_head = a.snapshot("incorrect origin", [i.published].into())?.id()?;
    req.examples = vec![Example {
        features: vec![plan],
        label: incorrect_id,
        partition: "train".into(),
    }];
    assert!(a.training_manifest(&req, f)?.withheld[&incorrect_id].contains("declared origin"));

    let notice = fixture::notice(&a, f, i.calibration, 80, 1)?;
    let corrected = a.snapshot("corrected", [i.published].into())?;
    req.label_head = corrected.id()?;
    req.examples[0].label = i.labels[2];
    assert!(a.training_manifest(&req, f)?.withheld[&i.labels[2]].contains("reassessment"));
    req.standing = LabelStanding::Historical;
    assert_eq!(a.training_manifest(&req, f)?.included, vec![i.labels[2]]);
    let rec = evaluation::assess(&a, &corrected, &i.queries[2])?;
    assert!(rec.applicable_notices.contains(&notice));
    assert_eq!(rec.result, Some(Standing::Refuted));
    let received = evaluation::deposit_receipt(&a, &corrected, &i.queries[2], &rec, f)?;
    let assessed = a.snapshot("corrected receipt", [corrected.id()?].into())?;
    let label = a.label(&assessed, received, "synthetic", None, None, f)?;
    req.label_head = a
        .snapshot("corrected label", [assessed.id()?].into())?
        .id()?;
    req.standing = LabelStanding::Current;
    req.examples[0].label = label;
    assert_eq!(a.training_manifest(&req, f)?.included, vec![label]);
    Ok(())
}

#[test]
fn prospective_training_requires_an_admitted_witness_and_concealed_outcomes() -> Result<()> {
    let f = Format::A;
    for signed in [false, true] {
        let a = Archive::memory()?;
        let i = fixture::seed(&a, f)?;
        let old = a.head(i.s0)?;
        let mut q = i.queries[2].clone();
        let mut arg = a.read(&old, q.arguments[0], "read")?;
        let plan = arg.one("plan")?;
        let outcome = arg.roles("reading").next().unwrap();
        let mut release = a.read(&old, arg.one("release")?, "read")?;
        let cutoff = if signed {
            a.blob(&serde_json::to_vec(&old)?, Id::blob(PUBLIC))?
        } else {
            i.feature_cutoff
        };
        release.inputs.retain(|e| e.role != "feature-cutoff");
        release.inputs.push(link("feature-cutoff", cutoff));
        release
            .fields
            .insert("source".into(), json!("replacement release declaration"));
        let witness = a.document(&release, f, Id::blob(PUBLIC))?;
        if signed {
            fixture::sign(&a, f, witness, &release.scope, "release", 6)?;
        }
        arg.inputs.retain(|e| e.role != "release");
        arg.inputs.push(link("release", witness));
        let revised = a.document(&arg, f, Id::blob(PUBLIC))?;
        fixture::sign(&a, f, revised, &release.scope, "data-origin", 7)?;
        q.arguments = vec![revised];
        let horizon = a.snapshot("assessment", [i.s0].into())?;
        let receipt = evaluation::assess(&a, &horizon, &q)?;
        assert_eq!(receipt.result, Some(Standing::Supported));
        let receipt_id = evaluation::deposit_receipt(&a, &horizon, &q, &receipt, f)?;
        let receipts = a.snapshot("receipts", [horizon.id()?].into())?;
        let label = a.label(&receipts, receipt_id, "synthetic", None, None, f)?;
        let labeled = a.snapshot("labeled", [receipts.id()?].into())?;
        let req = TrainingRequest {
            feature_cutoff: cutoff,
            label_head: labeled.id()?,
            policy: i.policy,
            standing: Default::default(),
            forecast_plan: Some(plan),
            mappings: vec![],
            project: None,
            independent_roles: BTreeSet::new(),
            binary: true,
            examples: vec![Example {
                features: vec![if signed { outcome } else { plan }],
                label,
                partition: "train".into(),
            }],
        };
        let result = a.training_manifest(&req, f)?;
        assert!(result.included.is_empty());
        assert_eq!(
            result.withheld[&label],
            if signed {
                "outcome evidence visible at the feature cutoff"
            } else {
                "no admitted release signature at the receipt horizon"
            }
        );
        let manifest = a.read(
            &a.snapshot("manifest", [labeled.id()?].into())?,
            result.manifest,
            "read",
        )?;
        assert_eq!(
            manifest.field::<String>("exposure")?,
            "no eligible examples"
        );
    }
    Ok(())
}

#[test]
fn signed_disciplinary_targets_survive_exchange_and_enforce_use_scope() -> Result<()> {
    let a = Archive::memory()?;
    let f = Format::A;
    let unrestricted = Id::blob(PUBLIC);
    let grant = |expires| {
        a.blob(
            &serde_json::to_vec(&json!({
                "version":1,"purposes":["read","train"],"training":{
                    "project":"methods-study","not_before":0,"expires":expires
                }
            }))?,
            unrestricted,
        )
    };
    let restricted = a.blob(b"synthetic baseline, one participant", grant(u64::MAX)?)?;
    let expired = a.blob(b"expired baseline", grant(1)?)?;
    let keys = |n| hex::encode(fixture::fixture_key(n).verifying_key().to_bytes());
    let policy = put(
        &a,
        f,
        Kind::Policy,
        "fixture policy",
        "research-policy-1",
        json!({"keys":{"data-origin":[keys(7)],"outcome":[keys(8)]}}),
        vec![],
    )?;
    let ready = a.snapshot("features", BTreeSet::new())?;
    let before = a.blob(&serde_json::to_vec(&ready)?, unrestricted)?;
    let cases = [
        (
            "clinical",
            "time_to_event",
            json!({"target_type":"time_to_event","unit":"day"}),
            json!({"kind":"time_to_event","time":30,"unit":"day","status":"right_censored"}),
        ),
        (
            "laboratory",
            "interval",
            json!({"target_type":"interval","unit":"mm","interval_meaning":"absolute_bound"}),
            json!({"kind":"interval","lower":150,"upper":200,"scale":3,"unit":"mm","interpretation":{"kind":"absolute_bound"}}),
        ),
        (
            "textual",
            "readings",
            json!({"target_type":"readings"}),
            json!({"kind":"readings","variants":[{"id":"A","text":"1480","selector":"#A-line-7"},{"id":"B","text":"1481","selector":"#B-line-7"}]}),
        ),
        (
            "missing",
            "time_to_event",
            json!({"target_type":"time_to_event","unit":"day"}),
            json!({"kind":"missing","reason":"not_sampled"}),
        ),
    ];
    let mut queries = vec![];
    for (subject, _, profile_fields, target) in &cases {
        let profile = put(
            &a,
            f,
            Kind::Profile,
            subject,
            "training-target-1",
            profile_fields.clone(),
            vec![],
        )?;
        let claim = put(
            &a,
            f,
            Kind::Claim,
            subject,
            "case",
            json!({"predicate":"training-target"}),
            vec![link("target-profile", profile)],
        )?;
        let mut sources = vec![link("target-profile", profile)];
        if *subject == "textual" {
            for (witness, bytes) in [("A", b"transcript A"), ("B", b"transcript B")] {
                let mut source = link("variant-source", a.blob(bytes, unrestricted)?);
                source.qualifier = witness.into();
                sources.push(source);
            }
        }
        let outcome = put(
            &a,
            f,
            Kind::Occurrence,
            subject,
            "case",
            json!({"target":target}),
            sources,
        )?;
        fixture::sign(&a, f, outcome, "case", "outcome", 8)?;
        let argument = put(
            &a,
            f,
            Kind::Argument,
            subject,
            "case",
            json!({"method":"typed-outcome","origin":"synthetic"}),
            vec![link("claim", claim), link("outcome", outcome)],
        )?;
        fixture::sign(&a, f, argument, "case", "data-origin", 7)?;
        queries.push(Query {
            target: claim,
            policy,
            arguments: vec![argument],
            budget: 1000,
            byte_limit: 1_048_576,
            mappings: vec![],
            previous_receipts: vec![],
            required_notices: vec![],
            current_at: None,
        });
    }
    let source = a.snapshot("typed source", [before].into())?;
    let migration = a.migrate(&source, Format::B, "fixture custodian")?;
    let mut b = Archive::memory()?;
    let exchange = a.export(&a.head(migration.exchange_head)?, "read")?;
    b.import(&exchange)?;
    let imported = b.head(migration.exchange_head)?;
    let mapping = b.verify_mapping(&imported, migration.mapping)?;
    let mut examples = vec![];
    for ((_, _, _, expected), q) in cases.iter().zip(&queries) {
        let query = Query {
            target: mapping[&q.target],
            policy: mapping[&policy],
            arguments: vec![mapping[&q.arguments[0]]],
            mappings: vec![migration.mapping],
            ..q.clone()
        };
        let receipt = evaluation::assess(&b, &imported, &query)?;
        assert_eq!(receipt.branches[0]["training_target"], *expected);
        assert_eq!(receipt.result, Some(Standing::Supported));
        let id = evaluation::deposit_receipt(&b, &imported, &query, &receipt, Format::B)?;
        let head = b.snapshot("typed receipts", [migration.exchange_head].into())?;
        let label = b.label(&head, id, "synthetic", None, None, Format::B)?;
        let labeled = b.snapshot("typed labels", [head.id()?].into())?;
        assert_eq!(b.read(&labeled, label, "read")?.fields["value"], *expected);
        examples.push(Example {
            features: vec![restricted],
            label,
            partition: "train".into(),
        });
    }
    let head = b.snapshot("ready", [migration.exchange_head].into())?;
    let mut req = TrainingRequest {
        feature_cutoff: before,
        label_head: head.id()?,
        policy: mapping[&policy],
        standing: Default::default(),
        forecast_plan: None,
        mappings: vec![migration.mapping],
        project: Some("methods-study".into()),
        examples,
        independent_roles: BTreeSet::new(),
        binary: false,
    };
    assert_eq!(b.training_manifest(&req, Format::B)?.included.len(), 4);
    req.binary = true;
    assert!(b.training_manifest(&req, Format::B)?.included.is_empty());
    req.binary = false;
    req.project = Some("different-study".into());
    assert!(b.training_manifest(&req, Format::B)?.included.is_empty());
    req.project = Some("methods-study".into());
    req.examples[0].features = vec![expired];
    assert_eq!(b.training_manifest(&req, Format::B)?.included.len(), 3);
    assert!(b.export(&head, "train")?.omitted.contains(&restricted));
    let query = &queries[1];
    let original_argument = a.read(&source, query.arguments[0], "read")?;
    let mut invalid_outcome = a.read(&source, original_argument.one("outcome")?, "read")?;
    invalid_outcome.fields.get_mut("target").unwrap()["interpretation"] =
        json!({"kind":"confidence","level_ppm":950000});
    let outcome = a.document(&invalid_outcome, f, unrestricted)?;
    fixture::sign(&a, f, outcome, "case", "outcome", 8)?;
    let mismatch = put(
        &a,
        f,
        Kind::Argument,
        "laboratory",
        "case",
        json!({"method":"typed-outcome","origin":"synthetic"}),
        vec![link("claim", query.target), link("outcome", outcome)],
    )?;
    let invalid = evaluation::assess(
        &a,
        &a.snapshot("mismatched semantics", [source.id()?].into())?,
        &Query {
            arguments: vec![mismatch],
            ..query.clone()
        },
    )?;
    assert_eq!(invalid.result, Some(Standing::Unresolved));
    assert!(
        invalid
            .obligations
            .contains("interval interpretation differs from profile")
    );

    Ok(())
}
