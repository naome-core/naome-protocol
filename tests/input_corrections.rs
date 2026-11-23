#![cfg(feature = "cli")]

use anyhow::Result;
use naome_protocol::{
    evaluation::{Query, Standing, Status, assess, deposit_receipt},
    fixture::{fixture_key, put, sign},
    learning::{Example, LabelStanding, TrainingRequest},
    profile::{Kind, Link, PUBLIC, link},
    store::Archive,
    wire::{Format, Id},
};
use serde_json::{Value, json};

const SUBJECT: &str = "threshold study";
const SCOPE: &str = "source publisher";
const ISSUER: u8 = 9;

struct Study<'a> {
    archive: &'a Archive,
    format: Format,
}

impl Study<'_> {
    fn record(&self, kind: Kind, fields: Value, links: Vec<Link>) -> Result<Id> {
        put(
            self.archive,
            self.format,
            kind,
            SUBJECT,
            SCOPE,
            fields,
            links,
        )
    }

    fn use_source(&self, version: Id, scope: &Value) -> Link {
        let mut input = link("source-input", version);
        if !scope.is_null() {
            input.qualifier = scope.to_string();
        }
        input
    }

    fn query(
        &self,
        claim: Id,
        policy: Id,
        version: Link,
        value: i64,
        audit: Vec<Link>,
    ) -> Result<Query> {
        let reading = self.record(
            Kind::Occurrence,
            json!({"positions":[0, value],"unit":"tick"}),
            vec![version],
        )?;
        let mut premises = vec![link("claim", claim), link("reading", reading)];
        premises.extend(audit);
        let argument = self.record(
            Kind::Argument,
            json!({"method":"recorded-difference","origin":"synthetic"}),
            premises,
        )?;
        sign(
            self.archive,
            self.format,
            argument,
            SCOPE,
            "data-origin",
            ISSUER,
        )?;
        Ok(Query {
            target: claim,
            arguments: vec![argument],
            policy,
            budget: 1_000,
            byte_limit: 1_048_576,
            mappings: vec![],
            previous_receipts: vec![],
            required_notices: vec![],
            current_at: None,
        })
    }

    fn notice(&self, version: Id, scope: &Value, evidence: Id, seed: u8) -> Result<(Id, Id)> {
        let notice = self.record(
            Kind::Notice,
            json!({"action":"correct-input","reason":"checked input revision","coverage":scope}),
            vec![link("source-version", version), link("evidence", evidence)],
        )?;
        let signature = sign(
            self.archive,
            self.format,
            notice,
            SCOPE,
            "input-correction",
            seed,
        )?;
        Ok((notice, signature))
    }
}

#[test]
fn profiles_share_correction_gates_and_preserve_historical_replay() -> Result<()> {
    let rows = |ranges| json!({"profile":"table-rows-1","selector":{"ranges":ranges,"columns":["Tot_Srvcs"]}});
    let ticks = |start, end, basis| {
        json!({
            "profile":"instrument-time-1",
            "selector":{"start":start,"end":end,"mode":"axial","time_basis":basis}
        })
    };
    for (changed, affected, outside, unknown) in [
        (
            rows(json!([[5775541, 5775542]])),
            rows(json!([[10, 15], [5775540, 5775543]])),
            rows(json!([[10, 15], [5775542, 5775544]])),
            rows(json!([[5775544, 5775542]])),
        ),
        (
            ticks(10, 20, "sample-clock-1"),
            ticks(9, 11, "sample-clock-1"),
            ticks(20, 30, "sample-clock-1"),
            ticks(20, 30, "unrelated-clock"),
        ),
    ] {
        for format in [Format::A, Format::B] {
            run_profile(format, &changed, &affected, &outside, &unknown)?;
        }
    }
    Ok(())
}

fn run_profile(
    format: Format,
    changed: &Value,
    affected: &Value,
    outside: &Value,
    unknown: &Value,
) -> Result<()> {
    let archive = Archive::memory()?;
    let study = Study {
        archive: &archive,
        format,
    };
    let public = Id::blob(PUBLIC);
    let old = archive.blob(b"immutable original series", public)?;
    let new = archive.blob(b"immutable revised series", public)?;
    let evidence = archive.blob(b"comparison identifies one erroneous value", public)?;
    let absent_evidence = Id::blob(b"missing evidence");
    let procedure = archive.blob(
        b"recompute selected raw input against the same threshold",
        public,
    )?;
    let key = hex::encode(fixture_key(ISSUER).verifying_key().to_bytes());
    let policy = put(
        &archive,
        format,
        Kind::Policy,
        SUBJECT,
        "research-policy-1",
        json!({"keys":{"data-origin":[key]},"input_correction_keys":{SCOPE:{(old.to_string()):[key]}}}),
        vec![],
    )?;
    let claim = study.record(
        Kind::Claim,
        json!({"predicate":"raw-change-above","unit":"tick","threshold":200}),
        vec![],
    )?;
    let affected_query =
        study.query(claim, policy, study.use_source(old, affected), 220, vec![])?;
    let outside_query = study.query(claim, policy, study.use_source(old, outside), 250, vec![])?;
    let coarse_query = study.query(claim, policy, link("dataset", old), 220, vec![])?;
    let unknown_query = study.query(claim, policy, study.use_source(old, unknown), 250, vec![])?;
    let original = archive.snapshot("as originally assessed", Default::default())?;
    let original_id = original.id()?;
    let original_receipt = assess(&archive, &original, &affected_query)?;
    assert_eq!(original_receipt.result, Some(Standing::Supported));
    let historical = deposit_receipt(
        &archive,
        &original,
        &affected_query,
        &original_receipt,
        format,
    )?;
    let before = archive.snapshot("original receipt", [original_id].into())?;
    let label = archive.label(&before, historical, "synthetic", None, None, format)?;
    let derived = study.record(
        Kind::Derivation,
        json!({"operation":"downstream review"}),
        vec![link("assessment", historical)],
    )?;
    let coarse_derived = study.record(
        Kind::Derivation,
        json!({"operation":"downstream review"}),
        vec![link("argument", coarse_query.arguments[0])],
    )?;
    let (unauthorized, _) = study.notice(old, changed, evidence, 11)?;
    let (wrong_source, _) = study.notice(new, changed, evidence, ISSUER)?;
    let untrusted = archive.snapshot("untrusted reports", [before.id()?].into())?;
    for notice in [unauthorized, wrong_source] {
        let result = assess(&archive, &untrusted, &affected_query)?;
        assert_eq!(result.result, Some(Standing::Supported));
        assert!(!result.applicable_notices.contains(&notice));
        assert_eq!(
            archive
                .impact_with_policy(&untrusted, notice, &[], 1000, Some(policy))?
                .correction_authority,
            None
        );
    }

    let (missing, _) = study.notice(old, changed, absent_evidence, ISSUER)?;
    let missing_head =
        archive.snapshot("unavailable correction evidence", [untrusted.id()?].into())?;
    let result = assess(&archive, &missing_head, &outside_query)?;
    assert_eq!(result.result, Some(Standing::Unresolved));
    assert!(
        result
            .checks
            .iter()
            .any(|c| c.status == Status::Unknown && c.inputs.contains(&missing))
    );
    // Exclude the incomplete candidate from the complete correction branch.
    let (admitted, authority) = study.notice(old, changed, evidence, ISSUER)?;
    let amended_head = archive.selected_snapshot(
        "admitted correction",
        [untrusted.id()?].into(),
        [
            admitted,
            authority,
            policy,
            derived,
            coarse_derived,
            outside_query.arguments[0],
            unknown_query.arguments[0],
            label,
        ]
        .into_iter()
        .chain(original.objects.iter().copied())
        .collect(),
    )?;
    assert!(!amended_head.objects.contains(&missing));
    let impact = archive.impact_with_policy(&amended_head, admitted, &[], 1000, Some(policy))?;
    assert!(impact.frontier.is_empty());
    assert_eq!(impact.correction_authority, Some(authority));
    assert_eq!(impact.by_kind["assessment"], [historical].into());
    assert_eq!(
        impact.outside_scope["argument"],
        [outside_query.arguments[0]].into()
    );
    assert_eq!(impact.by_kind["derivation"], [derived].into());
    assert_eq!(
        impact.by_kind["argument"],
        [affected_query.arguments[0]].into()
    );
    assert_eq!(
        impact.needs_reassessment["derivation"],
        [coarse_derived].into()
    );
    assert_eq!(
        impact.needs_reassessment["argument"],
        [coarse_query.arguments[0], unknown_query.arguments[0]].into()
    );
    for (query, status, standing) in [
        (&affected_query, Status::Pass, Standing::Unresolved),
        (&outside_query, Status::NotApplicable, Standing::Supported),
        (&coarse_query, Status::Unknown, Standing::Unresolved),
        (&unknown_query, Status::Unknown, Standing::Unresolved),
    ] {
        let mut query = query.clone();
        query.required_notices = vec![admitted];
        let receipt = assess(&archive, &amended_head, &query)?;
        assert_eq!(receipt.result, Some(standing), "{:?}", receipt.obligations);
        assert!(
            receipt
                .checks
                .iter()
                .any(|c| c.status == status && c.inputs.contains(&authority))
        );
    }
    let exact_old = archive.head(original_id)?;
    let replayed = assess(&archive, &exact_old, &affected_query)?;
    assert_eq!(
        serde_json::to_value(&replayed)?,
        serde_json::to_value(&original_receipt)?
    );
    assert_eq!(
        deposit_receipt(&archive, &exact_old, &affected_query, &replayed, format)?,
        historical
    );
    let mut training = TrainingRequest {
        feature_cutoff: original_id,
        label_head: amended_head.id()?,
        policy,
        standing: LabelStanding::Current,
        forecast_plan: None,
        mappings: vec![],
        project: None,
        examples: vec![Example {
            features: vec![procedure],
            label,
            partition: "train".into(),
        }],
        independent_roles: Default::default(),
        binary: true,
    };
    let current = archive.training_manifest(&training, format)?;
    assert!(current.withheld[&label].contains("reassessment"));
    training.standing = LabelStanding::Historical;
    assert_eq!(
        archive.training_manifest(&training, format)?.included,
        [label]
    );

    let revised = study.query(
        claim,
        policy,
        study.use_source(new, affected),
        190,
        vec![
            link("previous-result", historical),
            link("correction", admitted),
            link("procedure", procedure),
        ],
    )?;
    let revised_head = archive.selected_snapshot(
        "recomputed source",
        [amended_head.id()?].into(),
        [revised.arguments[0], policy, authority].into(),
    )?;
    let revised_receipt = assess(&archive, &revised_head, &revised)?;
    assert_eq!(revised_receipt.result, Some(Standing::Refuted));
    assert!(
        [new, historical, admitted, procedure]
            .iter()
            .all(|id| revised_receipt.dependencies.contains(id))
    );
    let revised_id = deposit_receipt(&archive, &revised_head, &revised, &revised_receipt, format)?;
    assert_ne!(revised_id, historical);
    Ok(())
}

fn unrelated_raw_reading_setup() -> Result<(Archive, Query, Id, Id, Id, Value)> {
    let archive = Archive::memory()?;
    let study = Study {
        archive: &archive,
        format: Format::B,
    };
    let public = Id::blob(PUBLIC);
    let old = archive.blob(b"original version of source A", public)?;
    let unrelated = archive.blob(b"unrelated provenance blob for raw reading B", public)?;
    let evidence = archive.blob(b"source A comparison", public)?;
    let key = hex::encode(fixture_key(ISSUER).verifying_key().to_bytes());
    let policy = put(
        &archive,
        Format::B,
        Kind::Policy,
        SUBJECT,
        "research-policy-1",
        json!({"keys":{"data-origin":[key]},"input_correction_keys":{SCOPE:{(old.to_string()):[key]}}}),
        vec![],
    )?;
    let claim = study.record(
        Kind::Claim,
        json!({"predicate":"raw-change-above","unit":"tick","threshold":200}),
        vec![],
    )?;
    let query = study.query(claim, policy, link("dataset", unrelated), 220, vec![])?;
    let coverage = json!({"profile":"table-rows-1","selector":{"ranges":[[1,2]]}});
    Ok((archive, query, old, unrelated, evidence, coverage))
}

#[test]
fn malformed_authorized_notice_cannot_block_unrelated_assessment() -> Result<()> {
    let (archive, query, old, unrelated, evidence, coverage) = unrelated_raw_reading_setup()?;
    let study = Study {
        archive: &archive,
        format: Format::B,
    };
    let dependent = study.query(
        query.target,
        query.policy,
        study.use_source(old, &coverage),
        220,
        vec![],
    )?;
    let before = archive.snapshot("original", Default::default())?;
    for candidate in [&query, &dependent] {
        assert_eq!(
            assess(&archive, &before, candidate)?.result,
            Some(Standing::Supported)
        );
    }
    let notice = study.record(
        Kind::Notice,
        json!({"action":"correct-input","reason":"checked input revision","coverage":coverage}),
        vec![
            link("source-version", old),
            link("evidence", evidence),
            link("calibration", unrelated),
        ],
    )?;
    sign(
        &archive,
        Format::B,
        notice,
        SCOPE,
        "input-correction",
        ISSUER,
    )?;
    let after = archive.snapshot("after malformed correction", [before.id()?].into())?;
    let result = assess(&archive, &after, &query)?;
    assert_eq!(result.result, Some(Standing::Supported));
    assert!(result.horizon_complete && result.frontier.is_empty());
    let impact = archive.impact_with_policy(&after, notice, &[], 1000, Some(query.policy))?;
    assert!(impact.by_kind.is_empty());
    assert_eq!(
        impact.needs_reassessment["argument"],
        [dependent.arguments[0]].into()
    );
    let result = assess(&archive, &after, &dependent)?;
    assert_eq!(result.result, Some(Standing::Unresolved));
    assert!(result.checks.iter().any(|check| {
        check.test == "input correction applicability"
            && check.status == Status::Unknown
            && check.inputs.contains(&notice)
    }));
    Ok(())
}

#[test]
fn unrelated_valid_notice_does_not_require_raw_provenance() -> Result<()> {
    let (archive, query, old, unrelated, evidence, coverage) = unrelated_raw_reading_setup()?;
    let study = Study {
        archive: &archive,
        format: Format::B,
    };
    archive.withdraw(unrelated)?;
    let before = archive.snapshot("after provenance withdrawal", Default::default())?;
    assert_eq!(
        assess(&archive, &before, &query)?.result,
        Some(Standing::Supported)
    );
    study.notice(old, &coverage, evidence, ISSUER)?;
    let after = archive.snapshot("after unrelated correction", [before.id()?].into())?;
    let result = assess(&archive, &after, &query)?;
    assert_eq!(result.result, Some(Standing::Supported));
    Ok(())
}

#[test]
fn missing_provenance_record_keeps_possible_correction_exposure_open() -> Result<()> {
    let (archive, query, old, _, evidence, coverage) = unrelated_raw_reading_setup()?;
    let study = Study {
        archive: &archive,
        format: Format::B,
    };
    let derivation = study.record(
        Kind::Derivation,
        json!({"method":"calculated a summary"}),
        vec![study.use_source(old, &coverage)],
    )?;
    let query = study.query(
        query.target,
        query.policy,
        link("dataset", derivation),
        220,
        vec![],
    )?;
    let mut before = archive.snapshot(
        "raw value without its provenance record",
        Default::default(),
    )?;
    assert!(before.objects.remove(&derivation));
    let original = assess(&archive, &before, &query)?;
    assert_eq!(original.result, Some(Standing::Supported));

    let (notice, _) = study.notice(old, &coverage, evidence, ISSUER)?;
    let mut after = archive.snapshot("after input correction", [archive.save(&before)?].into())?;
    assert!(after.objects.remove(&derivation));
    let current = assess(&archive, &after, &query)?;
    assert_eq!(current.result, Some(Standing::Unresolved));
    assert!(current.frontier.contains(&derivation));
    assert!(current.checks.iter().any(|check| {
        check.test == "input correction applicability"
            && check.status == Status::Unknown
            && check.inputs.contains(&notice)
    }));
    Ok(())
}

#[test]
fn unrelated_payloads_do_not_spend_the_notice_and_impact_record_budget() -> Result<()> {
    let (archive, mut query, source, _, evidence, coverage) = unrelated_raw_reading_setup()?;
    let study = Study {
        archive: &archive,
        format: Format::B,
    };
    let used = json!({"profile":"table-rows-1","selector":{"ranges":[[4,5]]}});
    query = study.query(
        query.target,
        query.policy,
        study.use_source(source, &used),
        220,
        vec![],
    )?;
    let (notice, signature) = study.notice(source, &coverage, evidence, ISSUER)?;
    query.required_notices.push(notice);
    let original = archive.snapshot("assessment inputs", Default::default())?;
    query.budget = original
        .objects
        .iter()
        .filter(|id| id.format().is_some())
        .count();

    let verify = |snapshot, query: &Query| -> Result<()> {
        let receipt = assess(&archive, snapshot, query)?;
        assert_eq!(receipt.result, Some(Standing::Supported));
        assert!(receipt.horizon_complete && receipt.frontier.is_empty());
        let impact =
            archive.impact_with_policy(snapshot, notice, &[], query.budget, Some(query.policy))?;
        assert_eq!(impact.correction_authority, Some(signature));
        assert!(impact.frontier.is_empty());
        assert!(impact.outside_scope["argument"].contains(&query.arguments[0]));
        let graph = archive.argument_graph(snapshot, query.target, "read", query.budget, 48)?;
        assert_eq!(graph["unsearched"], json!([]));
        Ok(())
    };
    verify(&original, &query)?;
    let payload: Vec<_> = (0u16..256).flat_map(u16::to_be_bytes).collect();
    archive.capture(
        payload.as_slice(),
        "application/octet-stream",
        2,
        Id::blob(PUBLIC),
        Format::B,
    )?;
    let larger = archive.snapshot("with separate dataset", [original.id()?].into())?;
    query.budget += 1; // The payload descriptor is itself a record.
    verify(&larger, &query)?;

    query.budget -= 1;
    let limited = assess(&archive, &larger, &query)?;
    assert_eq!(limited.result, Some(Standing::Unresolved));
    assert!(!limited.horizon_complete);
    assert_eq!(limited.frontier.len(), 1);
    assert!(limited.frontier.iter().all(|id| id.format().is_some()));
    Ok(())
}
