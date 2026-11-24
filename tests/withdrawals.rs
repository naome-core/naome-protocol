#![cfg(feature = "cli")]

use anyhow::{Context, Result};
use naome_protocol::{
    evaluation::{Query, Standing, Status, assess, deposit_receipt},
    fixture::{self, Investigation, put},
    learning::{Example, TrainingRequest},
    profile::{Kind, PUBLIC, link},
    store::Archive,
    wire::{Format, Id},
};
use serde_json::json;

const SUBJECT: &str = "bar-7";
const SCOPE: &str = "transition-1";

fn threshold_query(archive: &Archive, case: &Investigation, format: Format) -> Result<Query> {
    let mut query = case.queries[2].clone();
    let mut policy = archive.read(&archive.head(case.s0)?, query.policy, "read")?;
    policy
        .fields
        .get_mut("keys")
        .context("missing policy keys")?["notice"] = json!([hex::encode(
        fixture::fixture_key(9).verifying_key().to_bytes()
    )]);
    query.policy = archive.document(&policy, format, Id::blob(PUBLIC))?;
    Ok(query)
}

fn withdraw(archive: &Archive, format: Format, target: Id) -> Result<Id> {
    let notice = put(
        archive,
        format,
        Kind::Notice,
        SUBJECT,
        SCOPE,
        json!({"action":"withdraw","reason":"record withdrawn for this transition"}),
        vec![link("target", target)],
    )?;
    fixture::sign(archive, format, notice, SCOPE, "notice", 9)?;
    Ok(notice)
}

fn raw_claim(archive: &Archive, format: Format) -> Result<Id> {
    put(
        archive,
        format,
        Kind::Claim,
        SUBJECT,
        SCOPE,
        json!({"predicate":"raw-change-above","unit":"um","threshold":200}),
        vec![],
    )
}

fn raw_argument(
    archive: &Archive,
    format: Format,
    claim: Id,
    reading: Id,
    edition: u8,
) -> Result<Id> {
    put(
        archive,
        format,
        Kind::Argument,
        SUBJECT,
        SCOPE,
        json!({"method":"recorded-difference","edition":edition}),
        vec![link("claim", claim), link("reading", reading)],
    )
}

#[test]
fn withdrawing_the_only_correction_report_leaves_the_claim_unresolved() -> Result<()> {
    for format in [Format::A, Format::B] {
        let archive = Archive::memory()?;
        let case = fixture::seed(&archive, format)?;
        let query = threshold_query(&archive, &case, format)?;
        let original = archive.snapshot("original", Default::default())?;
        assert_eq!(
            assess(&archive, &original, &query)?.result,
            Some(Standing::Supported)
        );

        let correction = fixture::notice(&archive, format, case.calibration, 80, 1)?;
        let corrected = archive.snapshot("corrected", [original.id()?].into())?;
        let report = archive
            .read(&corrected, correction, "read")?
            .one("evidence")?;
        let refutation = assess(&archive, &corrected, &query)?;
        assert_eq!(refutation.result, Some(Standing::Refuted));
        assert!(refutation.dependencies.contains(&report));

        let withdrawal = withdraw(&archive, format, report)?;
        let current = archive.snapshot("report withdrawn", [corrected.id()?].into())?;
        let receipt = assess(&archive, &current, &query)?;
        assert_eq!(receipt.result, Some(Standing::Unresolved));
        assert!(receipt.horizon_complete && receipt.frontier.is_empty());
        assert_eq!(receipt.applicable_notices, [correction, withdrawal].into());
        assert!(receipt.branches.is_empty());
        assert!(!receipt.obligations.is_empty());
        assert_eq!(
            serde_json::to_value(assess(&archive, &corrected, &query)?)?,
            serde_json::to_value(refutation)?
        );
    }
    Ok(())
}

#[test]
fn a_rival_correction_survives_withdrawal_of_the_other_labs_report() -> Result<()> {
    let archive = Archive::memory()?;
    let format = Format::B;
    let case = fixture::seed(&archive, format)?;
    let query = threshold_query(&archive, &case, format)?;
    let first = fixture::notice(&archive, format, case.calibration, 80, 1)?;
    let second = fixture::notice(&archive, format, case.calibration, 10, 2)?;
    let rival = archive.snapshot("rival corrections", Default::default())?;
    assert_eq!(
        assess(&archive, &rival, &query)?.result,
        Some(Standing::Contested)
    );

    let report = archive.read(&rival, first, "read")?.one("evidence")?;
    let withdrawal = withdraw(&archive, format, report)?;
    let current = archive.snapshot("one report withdrawn", [rival.id()?].into())?;
    let receipt = assess(&archive, &current, &query)?;
    assert_eq!(receipt.result, Some(Standing::Supported));
    assert_eq!(
        receipt.applicable_notices,
        [first, second, withdrawal].into()
    );
    assert_eq!(receipt.branches.len(), 1);
    assert_eq!(receipt.branches[0]["notice"], json!(second));
    assert_eq!(receipt.branches[0]["joint"], json!([220, 270]));
    Ok(())
}

#[test]
fn calibration_withdrawal_preserves_recorded_values_but_blocks_physical_displacement() -> Result<()>
{
    for (format, legacy) in [(Format::A, false), (Format::B, true)] {
        let archive = Archive::memory()?;
        let case = if legacy {
            fixture::seed_legacy(&archive, format)?
        } else {
            fixture::seed(&archive, format)?
        };
        let physical = threshold_query(&archive, &case, format)?;
        let argument = archive.read(&archive.head(case.s0)?, physical.arguments[0], "read")?;
        let reading = argument
            .roles("reading")
            .next()
            .context("missing reading")?;
        let target = raw_claim(&archive, format)?;
        let raw = Query {
            target,
            arguments: vec![raw_argument(&archive, format, target, reading, 1)?],
            ..physical.clone()
        };
        let conditional = Query {
            arguments: vec![put(
                &archive,
                format,
                Kind::Argument,
                SUBJECT,
                SCOPE,
                json!({"method":"recorded-difference"}),
                vec![
                    link("claim", target),
                    link("reading", reading),
                    link("assumption", case.calibration),
                ],
            )?],
            ..raw.clone()
        };

        let before = archive.snapshot("calibrated", Default::default())?;
        let original = assess(&archive, &before, &physical)?;
        assert_eq!(original.result, Some(Standing::Supported));
        assert_eq!(
            assess(&archive, &before, &conditional)?.result,
            Some(Standing::Supported)
        );
        let withdrawal = withdraw(&archive, format, case.calibration)?;
        let after = archive.snapshot("calibration withdrawn", [before.id()?].into())?;
        let measurement = assess(&archive, &after, &physical)?;
        assert_eq!(measurement.result, Some(Standing::Unresolved));
        assert!(measurement.applicable_notices.contains(&withdrawal));
        assert!(measurement.branches.is_empty());

        let arithmetic = assess(&archive, &after, &raw)?;
        assert_eq!(arithmetic.result, Some(Standing::Supported));
        assert!(arithmetic.horizon_complete && arithmetic.obligations.is_empty());
        assert!(arithmetic.visible_notices.contains(&withdrawal));
        assert!(arithmetic.applicable_notices.is_empty());
        assert_eq!(arithmetic.branches, [json!({"raw_difference":"260"})]);
        let contingent = assess(&archive, &after, &conditional)?;
        assert_eq!(contingent.result, Some(Standing::Unresolved));
        assert!(contingent.applicable_notices.contains(&withdrawal));
        assert_eq!(
            serde_json::to_value(assess(&archive, &before, &physical)?)?,
            serde_json::to_value(original)?
        );
    }
    Ok(())
}

#[test]
fn only_arguments_using_the_withdrawn_reading_lose_support() -> Result<()> {
    let archive = Archive::memory()?;
    let format = Format::A;
    let case = fixture::seed(&archive, format)?;
    let mut query = threshold_query(&archive, &case, format)?;
    let argument = archive.read(&archive.head(case.s0)?, query.arguments[0], "read")?;
    let readings: Vec<_> = argument.roles("reading").collect();
    query.target = raw_claim(&archive, format)?;
    query.arguments = vec![
        raw_argument(&archive, format, query.target, readings[0], 1)?,
        raw_argument(&archive, format, query.target, readings[0], 2)?,
    ];
    let independent = raw_argument(&archive, format, query.target, readings[1], 1)?;
    let before = archive.snapshot("both readings", Default::default())?;
    assert_eq!(
        assess(&archive, &before, &query)?.result,
        Some(Standing::Supported)
    );
    withdraw(&archive, format, readings[0])?;
    let after = archive.snapshot("first reading withdrawn", [before.id()?].into())?;
    let shared = assess(&archive, &after, &query)?;
    assert_eq!(shared.result, Some(Standing::Unresolved));
    assert!(shared.branches.is_empty());
    query.arguments.push(independent);
    let surviving = assess(&archive, &after, &query)?;
    assert_eq!(surviving.result, Some(Standing::Supported));
    assert_eq!(surviving.branches, [json!({"raw_difference":"250"})]);
    Ok(())
}

/// Each instrument mode has a certificate derived from the same reference.
fn measurement_at(
    archive: &Archive,
    case: &Investigation,
    query: &Query,
    format: Format,
    mode: &str,
    event: i64,
) -> Result<Query> {
    let source = archive.head(case.s0)?;
    let public = Id::blob(PUBLIC);
    let mut certificate = archive.read(&source, case.calibration, "read")?;
    certificate.fields.insert("mode".into(), json!(mode));
    certificate.inputs.push(link("reference", case.calibration));
    let calibration = archive.document(&certificate, format, public)?;
    let mut argument = archive.read(&source, query.arguments[0], "read")?;
    argument.fields.insert("event".into(), json!(event));
    argument.fields.insert("mode".into(), json!(mode));
    for edge in &mut argument.inputs {
        match edge.role.as_str() {
            "calibration" => edge.target = calibration,
            "reading" | "guarantee" => {
                let mut input = archive.read(&source, edge.target, "read")?;
                if edge.role == "reading" {
                    input.fields.insert("mode".into(), json!(mode));
                    input.fields.insert("event".into(), json!(event));
                }
                for parent in &mut input.inputs {
                    if parent.role == "calibration" {
                        parent.target = calibration;
                    }
                }
                edge.target = archive.document(&input, format, public)?;
            }
            _ => {}
        }
    }
    Ok(Query {
        arguments: vec![archive.document(&argument, format, public)?],
        ..query.clone()
    })
}

#[test]
fn scoped_withdrawal_preserves_other_measurement_modes_and_events() -> Result<()> {
    for format in [Format::A, Format::B] {
        let archive = Archive::memory()?;
        let case = fixture::seed(&archive, format)?;
        let query = threshold_query(&archive, &case, format)?;
        let mut measurements = Vec::new();
        for (mode, event, affected) in [
            ("axial", 2, false),
            ("axial", 3, true),
            ("axial", 5, true),
            ("axial", 6, false),
            ("radial", 4, false),
        ] {
            measurements.push((
                measurement_at(&archive, &case, &query, format, mode, event)?,
                affected,
            ));
        }
        let outside_scope = put(
            &archive,
            format,
            Kind::Occurrence,
            SUBJECT,
            "another-transition",
            json!({"gauge":"A","mode":"axial","event":4}),
            vec![link("calibration", case.calibration)],
        )?;
        let before = archive.snapshot("calibrated", Default::default())?;
        let notice = put(
            &archive,
            format,
            Kind::Notice,
            SUBJECT,
            SCOPE,
            json!({"action":"withdraw","reason":"axial drift","mode":"axial","interval":[3,5]}),
            vec![link("target", case.calibration)],
        )?;
        fixture::sign(&archive, format, notice, SCOPE, "notice", 9)?;
        let after = archive.snapshot("limited withdrawal", [before.id()?].into())?;
        let impact = archive.impact(&after, notice, &[], after.objects.len())?;
        assert!(impact.frontier.is_empty());
        assert!(
            !impact
                .by_kind
                .get("occurrence")
                .is_some_and(|ids| ids.contains(&outside_scope))
        );
        for (mut query, affected) in measurements {
            assert_eq!(
                impact.by_kind["argument"].contains(&query.arguments[0]),
                affected
            );
            let prior = assess(&archive, &before, &query)?;
            assert_eq!(prior.result, Some(Standing::Supported));
            query.required_notices.push(notice);
            let receipt = assess(&archive, &after, &query)?;
            assert!(receipt.horizon_complete && receipt.visible_notices.contains(&notice));
            assert_eq!(receipt.applicable_notices.contains(&notice), affected);
            if affected {
                assert_eq!(receipt.result, Some(Standing::Unresolved));
                assert!(receipt.branches.is_empty());
            } else {
                assert_eq!(receipt.result, prior.result);
                assert_eq!(receipt.branches, prior.branches);
                assert!(receipt.obligations.is_empty());
            }
        }
    }
    Ok(())
}

#[test]
fn scoped_withdrawal_reaches_delegated_use_of_an_unaffected_receipt() -> Result<()> {
    for format in [Format::A, Format::B] {
        let archive = Archive::memory()?;
        let case = fixture::seed(&archive, format)?;
        let query = threshold_query(&archive, &case, format)?;
        let radial = measurement_at(&archive, &case, &query, format, "radial", 4)?;
        fixture::sign(
            &archive,
            format,
            radial.arguments[0],
            SCOPE,
            "data-origin",
            7,
        )?;
        let measured = archive.snapshot("radial evidence", Default::default())?;
        let original = assess(&archive, &measured, &radial)?;
        assert_eq!(original.result, Some(Standing::Supported));
        let receipt = deposit_receipt(&archive, &measured, &radial, &original, format)?;
        fixture::sign(&archive, format, receipt, SCOPE, "assessor", 4)?;
        let signed = archive.snapshot("signed radial receipt", [measured.id()?].into())?;
        let radial_label = archive.label(&signed, receipt, "synthetic", None, None, format)?;

        let argument = put(
            &archive,
            format,
            Kind::Argument,
            SUBJECT,
            SCOPE,
            json!({"method":"delegated","origin":"synthetic"}),
            vec![link("claim", radial.target), link("receipt", receipt)],
        )?;
        fixture::sign(&archive, format, argument, SCOPE, "data-origin", 7)?;
        let delegated = Query {
            arguments: vec![argument],
            ..radial.clone()
        };
        let before = archive.snapshot("delegated radial receipt", [measured.id()?].into())?;
        let original_delegate = assess(&archive, &before, &delegated)?;
        assert_eq!(original_delegate.result, Some(Standing::Supported));
        let delegated_receipt =
            deposit_receipt(&archive, &before, &delegated, &original_delegate, format)?;

        let assessed = archive.snapshot("delegated assessment", [before.id()?].into())?;
        let delegated_label = archive.label(
            &assessed,
            delegated_receipt,
            "synthetic",
            None,
            None,
            format,
        )?;
        let plan = archive
            .read(&before, radial.arguments[0], "read")?
            .one("plan")?;

        let notice = signed_withdrawal(
            &archive,
            format,
            case.calibration,
            json!({"mode":"axial","interval":[3,5]}),
        )?;
        let after = archive.snapshot("axial withdrawal", [assessed.id()?].into())?;
        let radial_recheck = assess(&archive, &after, &radial)?;
        assert_eq!(radial_recheck.result, Some(Standing::Supported));
        assert!(!radial_recheck.applicable_notices.contains(&notice));
        let delegated_recheck = assess(&archive, &after, &delegated)?;
        assert_eq!(delegated_recheck.result, Some(Standing::Unresolved));
        assert!(
            delegated_recheck
                .checks
                .iter()
                .any(|check| { check.status == Status::Unknown && check.inputs.contains(&notice) })
        );

        let mixed = Query {
            arguments: vec![radial.arguments[0], argument],
            required_notices: vec![notice],
            ..radial.clone()
        };
        let mixed_recheck = assess(&archive, &after, &mixed)?;
        let repeated = assess(&archive, &archive.head(after.id()?)?, &mixed)?;
        assert_eq!(
            deposit_receipt(&archive, &after, &mixed, &mixed_recheck, format)?,
            deposit_receipt(&archive, &after, &mixed, &repeated, format)?
        );
        assert_eq!(mixed_recheck.result, Some(Standing::Unresolved));
        assert!(!mixed_recheck.horizon_complete);
        assert!(
            mixed_recheck
                .obligations
                .contains("required notice applicability unknown")
        );

        let impact = archive.impact(&after, notice, &[], after.objects.len())?;
        assert!(impact.frontier.is_empty());
        assert!(
            impact
                .by_kind
                .get("argument")
                .is_some_and(|ids| ids.contains(&argument))
        );
        assert!(!impact.by_kind["argument"].contains(&radial.arguments[0]));
        assert!(impact.by_kind["assessment"].contains(&delegated_receipt));
        assert!(!impact.by_kind["assessment"].contains(&receipt));
        assert!(impact.by_kind["label"].contains(&delegated_label));
        assert!(!impact.by_kind["label"].contains(&radial_label));

        let mut request = TrainingRequest {
            feature_cutoff: before.id()?,
            label_head: after.id()?,
            policy: radial.policy,
            standing: Default::default(),
            forecast_plan: None,
            mappings: vec![],
            project: None,
            examples: [radial_label, delegated_label]
                .into_iter()
                .map(|label| Example {
                    features: vec![plan],
                    label,
                    partition: "train".into(),
                })
                .collect(),
            independent_roles: Default::default(),
            binary: true,
        };
        let current = archive.training_manifest(&request, format)?;
        assert_eq!(current.included, [radial_label]);
        assert!(current.withheld[&delegated_label].contains("reassessment"));

        request.standing = naome_protocol::learning::LabelStanding::Historical;
        let historical = archive.training_manifest(&request, format)?;
        assert_eq!(historical.included, [radial_label, delegated_label]);
        assert!(historical.withheld.is_empty());
        assert_eq!(
            deposit_receipt(
                &archive,
                &before,
                &delegated,
                &assess(&archive, &before, &delegated)?,
                format
            )?,
            delegated_receipt
        );
    }
    Ok(())
}

#[test]
fn direct_withdrawal_routes_affected_claims_and_arguments_without_a_prior_receipt() -> Result<()> {
    for format in [Format::A, Format::B] {
        let archive = Archive::memory()?;
        let case = fixture::seed(&archive, format)?;
        let before = archive.head(case.s0)?;
        let query = threshold_query(&archive, &case, format)?;
        let argument = query.arguments[0];
        let mut members = before.objects.clone();
        members.insert(query.policy);
        let before = archive.seal_members(&before.source, before.parents.clone(), members)?;
        assert_eq!(
            assess(&archive, &before, &query)?.result,
            Some(Standing::Supported)
        );

        for (target, kind, mode, affected) in [
            (argument, "argument", "axial", true),
            (query.target, "claim", "axial", true),
            (argument, "argument", "radial", false),
        ] {
            let notice = signed_withdrawal(&archive, format, target, json!({"mode":mode}))?;
            let signature = fixture::sign(&archive, format, notice, SCOPE, "notice", 9)?;
            let mut members = before.objects.clone();
            members.extend([notice, signature]);
            let after = archive.seal_members(&before.source, before.parents.clone(), members)?;
            let impact = archive.impact(&after, notice, &[], after.objects.len())?;
            assert!(impact.frontier.is_empty());
            assert_eq!(
                impact
                    .by_kind
                    .get(kind)
                    .is_some_and(|ids| ids.contains(&target)),
                affected
            );

            let required = Query {
                required_notices: vec![notice],
                ..query.clone()
            };
            let assessed = assess(&archive, &after, &required)?;
            assert!(assessed.horizon_complete);
            assert_eq!(assessed.applicable_notices.contains(&notice), affected);
            let expected = if affected {
                Standing::Unresolved
            } else {
                Standing::Supported
            };
            assert_eq!(assessed.result, Some(expected));
        }
    }
    Ok(())
}

#[test]
fn correction_evidence_withdrawals_use_the_measurement_context() -> Result<()> {
    let archive = Archive::memory()?;
    let format = Format::B;
    let case = fixture::seed(&archive, format)?;
    let query = threshold_query(&archive, &case, format)?;
    let correction = fixture::notice(&archive, format, case.calibration, 80, 1)?;
    let corrected = archive.snapshot("corrected", Default::default())?;
    let report = archive
        .read(&corrected, correction, "read")?
        .one("evidence")?;
    let withdraw_report = |mut fields: serde_json::Value| -> Result<Id> {
        fields["action"] = json!("withdraw");
        fields["reason"] = json!("limited laboratory withdrawal");
        let id = put(
            &archive,
            format,
            Kind::Notice,
            SUBJECT,
            SCOPE,
            fields,
            vec![link("target", report)],
        )?;
        fixture::sign(&archive, format, id, SCOPE, "notice", 9)?;
        Ok(id)
    };
    let outside = withdraw_report(json!({"mode":"radial"}))?;
    let radial = archive.snapshot("radial withdrawal", [corrected.id()?].into())?;
    let retained = assess(&archive, &radial, &query)?;
    assert_eq!(retained.result, Some(Standing::Refuted));
    assert_eq!(retained.applicable_notices, [correction].into());
    assert!(retained.visible_notices.contains(&outside));

    let inside = withdraw_report(json!({"interval":[1,1]}))?;
    let final_head = archive.snapshot("one event withdrawn", [radial.id()?].into())?;
    let affected = assess(&archive, &final_head, &query)?;
    assert_eq!(affected.result, Some(Standing::Unresolved));
    assert_eq!(affected.applicable_notices, [correction, inside].into());
    assert_eq!(
        assess(&archive, &radial, &query)?.branches,
        retained.branches
    );
    Ok(())
}

#[test]
fn incomplete_applicability_keeps_the_warrant_open() -> Result<()> {
    let archive = Archive::memory()?;
    let format = Format::A;
    let case = fixture::seed(&archive, format)?;
    let mut query = threshold_query(&archive, &case, format)?;
    query.target = raw_claim(&archive, format)?;
    let reading = put(
        &archive,
        format,
        Kind::Occurrence,
        SUBJECT,
        SCOPE,
        json!({"positions":[0,260],"unit":"um"}),
        vec![],
    )?;
    query.arguments = vec![raw_argument(&archive, format, query.target, reading, 1)?];
    let before = archive.snapshot("unqualified reading", Default::default())?;
    assert_eq!(
        assess(&archive, &before, &query)?.result,
        Some(Standing::Supported)
    );

    for (limits, reason) in [
        (
            json!({"mode":"axial"}),
            "withdrawal needs a measurement mode",
        ),
        (json!({"interval":[5,3]}), "reversed withdrawal interval"),
    ] {
        let mut fields = limits;
        fields["action"] = json!("withdraw");
        fields["reason"] = json!("restricted use");
        let notice = put(
            &archive,
            format,
            Kind::Notice,
            SUBJECT,
            SCOPE,
            fields,
            vec![link("target", reading)],
        )?;
        fixture::sign(&archive, format, notice, SCOPE, "notice", 9)?;
        let head = archive.snapshot("limited notice", [before.id()?].into())?;
        let impact = archive.impact(&head, notice, &[], head.objects.len())?;
        assert!(impact.by_kind["argument"].contains(&query.arguments[0]));
        let required = Query {
            required_notices: vec![notice],
            ..query.clone()
        };
        let receipt = assess(&archive, &head, &required)?;
        assert_eq!(receipt.result, Some(Standing::Unresolved));
        assert!(!receipt.horizon_complete);
        assert!(!receipt.applicable_notices.contains(&notice));
        assert!(
            receipt
                .checks
                .iter()
                .any(|c| c.status == naome_protocol::evaluation::Status::Unknown
                    && c.inputs.contains(&notice)
                    && c.basis == reason)
        );
    }
    Ok(())
}

#[test]
fn a_known_mismatch_excludes_a_withdrawal_with_an_invalid_restriction() -> Result<()> {
    for fields in [
        json!({"mode":"radial","interval":[5,3]}),
        json!({"mode":"","interval":[2,3]}),
    ] {
        let archive = Archive::memory()?;
        let format = Format::A;
        let case = fixture::seed(&archive, format)?;
        let query = threshold_query(&archive, &case, format)?;
        let before = archive.snapshot("before", Default::default())?;
        let original = assess(&archive, &before, &query)?;
        assert_eq!(original.result, Some(Standing::Supported));
        let notice = signed_withdrawal(&archive, format, case.calibration, fields)?;
        let after = archive.snapshot("withdrawal", [before.id()?].into())?;
        let result = assess(&archive, &after, &query)?;
        assert_eq!(result.result, original.result);
        assert_eq!(result.branches, original.branches);
        assert!(
            result.checks.iter().any(
                |check| check.status == Status::NotApplicable && check.inputs.contains(&notice)
            )
        );
    }
    Ok(())
}

#[test]
fn a_recorded_empty_mode_excludes_a_withdrawal_in_a_named_mode() -> Result<()> {
    for format in [Format::A, Format::B] {
        let archive = Archive::memory()?;
        let case = fixture::seed(&archive, format)?;
        let original_query = threshold_query(&archive, &case, format)?;
        let query = measurement_at(&archive, &case, &original_query, format, "", 1)?;
        let before = archive.snapshot("empty recorded mode", Default::default())?;
        let original = assess(&archive, &before, &query)?;
        assert_eq!(original.result, Some(Standing::Supported));
        let notice =
            signed_withdrawal(&archive, format, case.calibration, json!({"mode":"axial"}))?;
        let after = archive.snapshot("axial withdrawal", [before.id()?].into())?;
        let result = assess(&archive, &after, &query)?;
        assert_eq!(result.result, original.result);
        assert_eq!(result.branches, original.branches);
        assert!(
            result.checks.iter().any(
                |check| check.status == Status::NotApplicable && check.inputs.contains(&notice)
            )
        );
    }
    Ok(())
}

#[test]
fn scoped_withdrawal_rechecks_label_and_feature_uses_independently() -> Result<()> {
    let archive = Archive::memory()?;
    let format = Format::A;
    let case = fixture::seed(&archive, format)?;
    let original = threshold_query(&archive, &case, format)?;
    let radial = measurement_at(&archive, &case, &original, format, "radial", 1)?;
    fixture::sign(
        &archive,
        format,
        radial.arguments[0],
        SCOPE,
        "data-origin",
        7,
    )?;
    let before = archive.snapshot("before", Default::default())?;
    let plan = archive
        .read(&before, original.arguments[0], "read")?
        .one("plan")?;

    let make_label = |query: &Query| -> Result<Id> {
        let result = assess(&archive, &before, query)?;
        assert_eq!(result.result, Some(Standing::Supported));
        let id = deposit_receipt(&archive, &before, query, &result, format)?;
        let assessed = archive.snapshot("assessed", [before.id()?].into())?;
        archive.label(&assessed, id, "synthetic", None, None, format)
    };
    let axial_label = make_label(&original)?;
    let radial_label = make_label(&radial)?;
    let labeled = archive.snapshot("labeled", [before.id()?].into())?;

    let mut request = TrainingRequest {
        feature_cutoff: before.id()?,
        label_head: labeled.id()?,
        policy: radial.policy,
        standing: Default::default(),
        forecast_plan: None,
        mappings: vec![],
        project: None,
        examples: vec![Example {
            features: vec![plan],
            label: radial_label,
            partition: "train".into(),
        }],
        independent_roles: Default::default(),
        binary: true,
    };
    assert_eq!(
        archive.training_manifest(&request, format)?.included,
        [radial_label]
    );
    signed_withdrawal(
        &archive,
        format,
        case.calibration,
        json!({"mode":"axial","interval":[1,1]}),
    )?;
    request.label_head = archive
        .snapshot("axial withdrawal", [labeled.id()?].into())?
        .id()?;
    assert_eq!(
        archive.training_manifest(&request, format)?.included,
        [radial_label]
    );

    request.examples[0].label = axial_label;
    assert!(
        archive.training_manifest(&request, format)?.withheld[&axial_label]
            .contains("reassessment")
    );

    request.examples[0].label = radial_label;
    request.examples[0].features.push(case.calibration);
    assert!(
        archive.training_manifest(&request, format)?.withheld[&radial_label]
            .contains("training dependency")
    );
    Ok(())
}

fn signed_withdrawal(
    archive: &Archive,
    format: Format,
    target: Id,
    mut fields: serde_json::Value,
) -> Result<Id> {
    fields["action"] = json!("withdraw");
    fields["reason"] = json!("limited calibration use");
    let id = put(
        archive,
        format,
        Kind::Notice,
        SUBJECT,
        SCOPE,
        fields,
        vec![link("target", target)],
    )?;
    fixture::sign(archive, format, id, SCOPE, "notice", 9)?;
    Ok(id)
}
