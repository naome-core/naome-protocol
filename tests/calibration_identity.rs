#![cfg(feature = "cli")]

use anyhow::{Context, Result};
use naome_protocol::{
    evaluation::{Query, Standing, Status, assess, deposit_receipt},
    fixture::{self, put},
    learning::{Example, LabelStanding, TrainingRequest},
    profile::{Kind, Link, PUBLIC, link},
    store::{Archive, Snapshot},
    wire::{Format, Id},
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, process::Command};

const SUBJECT: &str = "bar-7";
const LOCAL: &str = "transition-1";
const FOREIGN: &str = "calibration-custodian-north";
const TEST: &str = "calibration-event-identity";

fn key(n: u8) -> String {
    hex::encode(fixture::fixture_key(n).verifying_key().to_bytes())
}

fn record(
    archive: &Archive,
    kind: Kind,
    scope: &str,
    fields: Value,
    links: Vec<Link>,
) -> Result<Id> {
    put(archive, Format::B, kind, SUBJECT, scope, fields, links)
}

fn manifest(archive: &Archive, snapshot: &Snapshot) -> Result<Id> {
    archive.deposit_manifest(snapshot)
}

struct Scenario {
    archive: Archive,
    before: Snapshot,
    imported: Snapshot,
    source_calibration: Id,
    source_original: Id,
    source_inventory: Id,
    local_inventory: Id,
    local_calibration: Id,
    local_original: Id,
    repeat_calibration: Id,
    repeat_original: Id,
    correction: Id,
    foreign_untrusted: Id,
    measured: Query,
    outside: Query,
    repeat: Query,
}

impl Scenario {
    fn new() -> Result<Self> {
        Self::with_recipient_axis("fixture-transition-axis-v1")
    }

    fn with_recipient_axis(axis: &str) -> Result<Self> {
        let sender = Archive::memory()?;
        let source_original = put(
            &sender,
            Format::A,
            Kind::Occurrence,
            "service ticket 32",
            FOREIGN,
            json!({"physical_time":"2026-05-01T11:42:23Z","serial":"SER-61",
                   "reference":"M3","execution":"longitudinal, ascending"}),
            vec![],
        )?;
        let source_calibration = put(
            &sender,
            Format::A,
            Kind::Calibration,
            "transducer Q",
            FOREIGN,
            json!({"mode":"axial", "interval":[1, 8], "bias":0, "unit":"um",
                   "event_time_basis":"fixture-transition-axis-v1"}),
            vec![],
        )?;
        let inspection = sender.snapshot("North calibration laboratory", Default::default())?;
        let source_inventory = manifest(&sender, &inspection)?;
        let correction = exportable_notice(&sender, source_calibration, 1, 80)?;
        let foreign_untrusted = exportable_notice(&sender, source_calibration, 2, 100)?;
        let export = sender.snapshot("North notices", [inspection.id()?].into())?;

        let public = Id::blob(PUBLIC);
        let mut archive = Archive::memory()?;
        let case = fixture::seed(&archive, Format::B)?;
        let seed = archive.head(case.s0)?;
        let mut policy = archive.read(&seed, case.policy, "read")?;
        policy.fields.get_mut("keys").context("keys")?["notice"] = json!([key(9)]);
        policy.fields.get_mut("keys").context("keys")?["calibration-identity"] = json!([key(8)]);
        policy
            .fields
            .insert("foreign_calibration_keys".into(), json!({FOREIGN:[key(1)]}));
        let policy = archive.document(&policy, Format::B, public)?;

        let local_original = record(
            &archive,
            Kind::Occurrence,
            LOCAL,
            json!({"asset_alias":"bench Q","local_minute":"13:42 +0200","standard":"M3",
                   "configuration":"positive axial approach"}),
            vec![],
        )?;
        let repeat_original = record(
            &archive,
            Kind::Occurrence,
            LOCAL,
            json!({"asset_alias":"bench Q","local_minute":"13:42 +0200","standard":"M3",
                   "configuration":"negative axial approach"}),
            vec![],
        )?;
        let mut local = archive.read(&seed, case.calibration, "read")?;
        local.fields.insert("event_time_basis".into(), json!(axis));
        local.inputs.push(link("original", local_original));
        let local_calibration = archive.document(&local, Format::B, public)?;
        local.inputs = vec![link("original", repeat_original)];
        let repeat_calibration = archive.document(&local, Format::B, public)?;

        let use_at = |calibration, event| -> Result<Query> {
            let mut query = case.queries[2].clone();
            query.policy = policy;
            let mut argument = archive.read(&seed, query.arguments[0], "read")?;
            argument.fields.insert("event".into(), json!(event));
            for edge in &mut argument.inputs {
                match edge.role.as_str() {
                    "calibration" => edge.target = calibration,
                    "reading" | "guarantee" => {
                        let mut input = archive.read(&seed, edge.target, "read")?;
                        if edge.role == "reading" {
                            input.fields.insert("event".into(), json!(event));
                        }
                        input
                            .inputs
                            .iter_mut()
                            .filter(|parent| parent.role == "calibration")
                            .for_each(|parent| parent.target = calibration);
                        edge.target = archive.document(&input, Format::B, public)?;
                    }
                    _ => (),
                }
            }
            query.arguments = vec![archive.document(&argument, Format::B, public)?];
            Ok(query)
        };
        let measured = use_at(local_calibration, 1)?;
        let outside = use_at(local_calibration, 2)?;
        let repeat = use_at(repeat_calibration, 1)?;
        fixture::sign(
            &archive,
            Format::B,
            measured.arguments[0],
            LOCAL,
            "data-origin",
            7,
        )?;
        let local_archive = archive.snapshot("South measurement laboratory", Default::default())?;
        let local_inventory = manifest(&archive, &local_archive)?;
        let before = archive.snapshot("South before the notice", [local_archive.id()?].into())?;
        archive.import(&sender.export(&export, "read")?)?;
        let imported = archive.snapshot("South with North evidence", [before.id()?].into())?;
        Ok(Self {
            archive,
            before,
            imported,
            source_calibration,
            source_original,
            source_inventory,
            local_inventory,
            local_calibration,
            local_original,
            repeat_calibration,
            repeat_original,
            correction,
            foreign_untrusted,
            measured,
            outside,
            repeat,
        })
    }

    fn identity(
        &self,
        target: Id,
        evidence: Id,
        granularity: &str,
        coverage: &str,
        status: Status,
        signer: u8,
    ) -> Result<Id> {
        let claim = record(
            &self.archive,
            Kind::Claim,
            LOCAL,
            json!({"predicate":TEST,"granularity":granularity}),
            vec![
                link("source-calibration", self.source_calibration),
                link("recipient-calibration", target),
            ],
        )?;
        let assessment = record(
            &self.archive,
            Kind::Assessment,
            LOCAL,
            json!({"test":TEST,"status":status,"coverage":coverage,
                   "basis":"Serial register and positive approach distinguish the supported execution."}),
            vec![
                link("claim", claim),
                link("source-evidence", self.source_original),
                link("recipient-evidence", evidence),
                link("source-inventory", self.source_inventory),
                link("recipient-inventory", self.local_inventory),
            ],
        )?;
        fixture::sign(
            &self.archive,
            Format::B,
            assessment,
            LOCAL,
            "calibration-identity",
            signer,
        )?;
        Ok(assessment)
    }

    fn head(&self) -> Result<Snapshot> {
        self.archive
            .snapshot("South reassessment", [self.imported.id()?].into())
    }

    fn impact(&self, head: &Snapshot) -> Result<BTreeSet<Id>> {
        let result = self.archive.impact_with_policy(
            head,
            self.correction,
            &[],
            10_000,
            Some(self.measured.policy),
        )?;
        assert!(result.frontier.is_empty());
        Ok(result.by_kind.get("argument").cloned().unwrap_or_default())
    }
}

fn exportable_notice(archive: &Archive, calibration: Id, signer: u8, bias: i64) -> Result<Id> {
    let report = put(
        archive,
        Format::A,
        Kind::Occurrence,
        "North readback",
        FOREIGN,
        json!({"bias":bias,"unit":"um"}),
        vec![],
    )?;
    let notice = put(
        archive,
        Format::A,
        Kind::Notice,
        "North correction",
        FOREIGN,
        json!({"bias":bias,"unit":"um","mode":"axial","interval":[1,1]}),
        vec![link("calibration", calibration), link("evidence", report)],
    )?;
    fixture::sign(archive, Format::A, notice, FOREIGN, "calibration", signer)?;
    Ok(notice)
}

#[test]
fn admitted_identity_changes_only_the_dependent_in_interval_use() -> Result<()> {
    let trial = Scenario::new()?;
    let a = &trial.archive;
    let before = assess(a, &trial.before, &trial.measured)?;
    assert_eq!(before.result, Some(Standing::Supported));
    let mut required = trial.measured.clone();
    required.required_notices = vec![trial.correction];
    assert_eq!(
        assess(a, &trial.imported, &required)?.result,
        Some(Standing::Unresolved)
    );
    assert!(trial.impact(&trial.imported)?.is_empty());

    let identity = trial.identity(
        trial.local_calibration,
        trial.local_original,
        "execution",
        "complete",
        Status::Pass,
        8,
    )?;
    let after = trial.head()?;
    let current = assess(a, &after, &required)?;
    assert!(current.horizon_complete);
    assert_eq!(current.result, Some(Standing::Refuted));
    assert_eq!(current.applicable_notices, [trial.correction].into());
    assert!(current.dependencies.contains(&identity));
    assert!(current.dependencies.contains(&trial.source_original));
    assert!(current.dependencies.contains(&trial.local_original));
    assert_eq!(current.branches[0]["joint"], json!([150, 200]));
    assert_eq!(trial.impact(&after)?, [trial.measured.arguments[0]].into());
    assert!(
        !a.impact(&after, trial.correction, &[], 10_000)?
            .by_kind
            .contains_key("argument")
    );

    let mut outside = trial.outside.clone();
    outside.required_notices = vec![trial.correction];
    let outside = assess(a, &after, &outside)?;
    assert_eq!(outside.result, Some(Standing::Supported));
    assert!(outside.horizon_complete && outside.applicable_notices.is_empty());
    let repeat = assess(a, &after, &trial.repeat)?;
    assert_eq!(repeat.result, Some(Standing::Supported));
    assert!(repeat.applicable_notices.is_empty());

    required.required_notices = vec![trial.foreign_untrusted];
    assert_eq!(
        assess(a, &after, &required)?.result,
        Some(Standing::Unresolved)
    );
    assert_eq!(
        serde_json::to_value(assess(a, &trial.before, &trial.measured)?)?,
        serde_json::to_value(before)?
    );
    Ok(())
}

#[test]
fn incomplete_session_unauthorized_and_competing_assertions_leave_identity_open() -> Result<()> {
    let trial = Scenario::new()?;
    let mut query = trial.measured.clone();
    query.required_notices = vec![trial.correction];
    for (granularity, coverage, status, signer) in [
        ("session", "complete", Status::Pass, 8),
        ("execution", "partial", Status::Pass, 8),
        ("execution", "complete", Status::Unknown, 8),
        ("execution", "complete", Status::Pass, 4),
    ] {
        trial.identity(
            trial.local_calibration,
            trial.local_original,
            granularity,
            coverage,
            status,
            signer,
        )?;
        let head = trial.head()?;
        let result = assess(&trial.archive, &head, &query)?;
        assert_eq!(result.result, Some(Standing::Unresolved));
        assert!(result.applicable_notices.is_empty());
        assert!(trial.impact(&head)?.is_empty());
    }
    trial.identity(
        trial.local_calibration,
        trial.local_original,
        "execution",
        "complete",
        Status::Pass,
        8,
    )?;
    trial.identity(
        trial.repeat_calibration,
        trial.repeat_original,
        "execution",
        "complete",
        Status::Pass,
        8,
    )?;
    let head = trial.head()?;
    assert_eq!(
        assess(&trial.archive, &head, &query)?.result,
        Some(Standing::Unresolved)
    );
    assert!(trial.impact(&head)?.is_empty());
    Ok(())
}

#[test]
fn later_identity_withdrawal_keeps_historical_receipt_and_blocks_required_correction() -> Result<()>
{
    let trial = Scenario::new()?;
    let a = &trial.archive;
    let identity = trial.identity(
        trial.local_calibration,
        trial.local_original,
        "execution",
        "complete",
        Status::Pass,
        8,
    )?;
    let before = trial.head()?;
    let mut query = trial.measured.clone();
    query.required_notices = vec![trial.correction];
    let receipt = assess(a, &before, &query)?;
    assert_eq!(receipt.result, Some(Standing::Refuted));
    let receipt_id = deposit_receipt(a, &before, &query, &receipt, Format::B)?;
    let receipt_inventory = a.snapshot("receipt", [before.id()?].into())?;
    assert_eq!(
        a.read(&receipt_inventory, receipt_id, "read")?.fields["evaluator"],
        json!("rep-rust-rules-9")
    );
    let label = a.label(
        &receipt_inventory,
        receipt_id,
        "synthetic",
        None,
        None,
        Format::B,
    )?;
    let withdrawal = record(
        a,
        Kind::Notice,
        LOCAL,
        json!({"action":"withdraw","reason":"equipment register corrected"}),
        vec![link("target", identity)],
    )?;
    fixture::sign(a, Format::B, withdrawal, LOCAL, "notice", 9)?;
    let after = trial.head()?;
    let result = assess(a, &after, &query)?;
    assert_eq!(result.result, Some(Standing::Unresolved));
    assert!(trial.impact(&after)?.is_empty());
    assert_eq!(
        serde_json::to_value(assess(a, &before, &query)?)?,
        serde_json::to_value(receipt)?
    );

    let mut request = TrainingRequest {
        feature_cutoff: trial.before.id()?,
        forecast_plan: None,
        mappings: vec![],
        label_head: after.id()?,
        policy: query.policy,
        standing: LabelStanding::Historical,
        project: None,
        examples: vec![Example {
            features: vec![query.target],
            label,
            partition: "train".into(),
        }],
        independent_roles: Default::default(),
        binary: true,
    };
    let historical = a.training_manifest(&request, Format::B)?;
    assert_eq!(historical.included, [label], "{:?}", historical.withheld);
    request.standing = LabelStanding::Current;
    let current = a.training_manifest(&request, Format::B)?;
    assert!(current.included.is_empty());
    assert!(current.withheld[&label].contains("requires reassessment"));
    Ok(())
}

#[test]
fn cli_assesses_and_traces_after_import_into_fresh_archive() -> Result<()> {
    let trial = Scenario::new()?;
    trial.identity(
        trial.local_calibration,
        trial.local_original,
        "execution",
        "complete",
        Status::Pass,
        8,
    )?;
    let head = trial.head()?;
    let directory = tempfile::tempdir()?;
    let file = directory.path().join("bundle.json");
    fs::write(
        &file,
        serde_json::to_vec(&trial.archive.export(&head, "read")?)?,
    )?;
    let query = directory.path().join("query.json");
    fs::write(&query, serde_json::to_vec(&trial.measured)?)?;
    let cmd = |args: &[&str]| -> Result<Value> {
        let output = Command::new(env!("CARGO_BIN_EXE_nao"))
            .arg("--db")
            .arg(directory.path().join("recipient.sqlite"))
            .args(args)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    };
    let head = head.id()?.to_string();
    cmd(&["import", file.to_str().context("bundle path")?])?;
    let assessment = cmd(&["assess", &head, query.to_str().context("query path")?])?;
    assert_eq!(assessment["receipt"]["result"], json!("refuted"));
    let impact = cmd(&[
        "impact",
        &head,
        &trial.correction.to_string(),
        "--policy",
        &trial.measured.policy.to_string(),
    ])?;
    assert_eq!(
        impact["by_kind"]["argument"],
        json!(trial.measured.arguments)
    );
    Ok(())
}

#[test]
fn source_withdrawal_preserves_the_identity_and_excludes_other_uses() -> Result<()> {
    let trial = Scenario::new()?;
    let a = &trial.archive;
    let identity = trial.identity(
        trial.local_calibration,
        trial.local_original,
        "execution",
        "complete",
        Status::Pass,
        8,
    )?;
    let withdrawal = put(
        a,
        Format::A,
        Kind::Notice,
        "North withdrawal",
        FOREIGN,
        json!({"action":"withdraw","reason":"reference was out of tolerance","mode":"axial","interval":[1,1]}),
        vec![link("target", trial.source_calibration)],
    )?;
    fixture::sign(a, Format::A, withdrawal, FOREIGN, "notice", 1)?;
    let head = trial.head()?;
    let mut query = trial.measured.clone();
    query.required_notices = vec![withdrawal];
    let result = assess(a, &head, &query)?;
    assert_eq!(result.result, Some(Standing::Unresolved));
    assert!(result.horizon_complete);
    assert!(result.applicable_notices.contains(&withdrawal));
    assert!(result.dependencies.contains(&identity));
    let mut outside = trial.outside.clone();
    outside.required_notices = vec![withdrawal];
    let outside = assess(a, &head, &outside)?;
    assert_eq!(outside.result, Some(Standing::Supported));
    assert!(outside.horizon_complete && outside.applicable_notices.is_empty());
    let impact = a.impact_with_policy(&head, withdrawal, &[], 10_000, Some(query.policy))?;
    assert_eq!(impact.by_kind["argument"], [query.arguments[0]].into());
    assert!(
        !impact
            .by_kind
            .get("assessment")
            .is_some_and(|ids| ids.contains(&identity))
    );
    Ok(())
}

#[test]
fn a_signed_link_with_incompatible_time_axes_leaves_applicability_open() -> Result<()> {
    let trial = Scenario::with_recipient_axis("independent-local-counter")?;
    trial.identity(
        trial.local_calibration,
        trial.local_original,
        "execution",
        "complete",
        Status::Pass,
        8,
    )?;
    let head = trial.head()?;
    let mut query = trial.measured.clone();
    query.required_notices = vec![trial.correction];
    let receipt = assess(&trial.archive, &head, &query)?;
    assert_eq!(receipt.result, Some(Standing::Unresolved));
    assert!(receipt.applicable_notices.is_empty());
    assert!(trial.impact(&head)?.is_empty());
    Ok(())
}

#[test]
fn foreign_withdrawal_reaches_local_receipt_and_label() -> Result<()> {
    let trial = Scenario::new()?;
    let archive = &trial.archive;
    let historical = assess(archive, &trial.before, &trial.measured)?;
    assert_eq!(historical.result, Some(Standing::Supported));
    let receipt = deposit_receipt(
        archive,
        &trial.before,
        &trial.measured,
        &historical,
        Format::B,
    )?;
    let label_horizon = archive.snapshot("deposited local result", [trial.before.id()?].into())?;
    let label = archive.label(&label_horizon, receipt, "synthetic", None, None, Format::B)?;
    trial.identity(
        trial.local_calibration,
        trial.local_original,
        "execution",
        "complete",
        Status::Pass,
        8,
    )?;
    let withdrawal = put(
        archive,
        Format::A,
        Kind::Notice,
        "North withdrawal",
        FOREIGN,
        json!({"action":"withdraw","reason":"reference was out of tolerance","mode":"axial","interval":[1,1]}),
        vec![link("target", trial.source_calibration)],
    )?;
    fixture::sign(archive, Format::A, withdrawal, FOREIGN, "notice", 1)?;
    let head = trial.head()?;
    let mut query = trial.measured.clone();
    query.required_notices = vec![withdrawal];
    let current = assess(archive, &head, &query)?;
    assert_eq!(current.result, Some(Standing::Unresolved));
    assert!(current.applicable_notices.contains(&withdrawal));
    let impact = archive.impact_with_policy(&head, withdrawal, &[], 10_000, Some(query.policy))?;
    assert!(impact.by_kind["argument"].contains(&query.arguments[0]));
    assert!(
        impact
            .by_kind
            .get("assessment")
            .is_some_and(|ids| ids.contains(&receipt))
    );
    assert!(
        impact
            .by_kind
            .get("label")
            .is_some_and(|ids| ids.contains(&label))
    );
    Ok(())
}

fn delegate(
    archive: &Archive,
    snapshot: &Snapshot,
    query: &Query,
    standing: Standing,
) -> Result<Query> {
    let original = assess(archive, snapshot, query)?;
    assert_eq!(original.result, Some(standing));
    let receipt = deposit_receipt(archive, snapshot, query, &original, Format::B)?;
    fixture::sign(archive, Format::B, receipt, LOCAL, "assessor", 4)?;
    let argument = record(
        archive,
        Kind::Argument,
        LOCAL,
        json!({"method":"delegated"}),
        vec![link("claim", query.target), link("receipt", receipt)],
    )?;
    Ok(Query {
        arguments: vec![argument],
        ..query.clone()
    })
}

#[test]
fn delegation_reassesses_only_the_use_affected_by_a_linked_correction() -> Result<()> {
    let trial = Scenario::new()?;
    let archive = &trial.archive;
    let affected = delegate(archive, &trial.before, &trial.measured, Standing::Supported)?;
    let outside = delegate(archive, &trial.before, &trial.outside, Standing::Supported)?;
    let before_identity = trial.head()?;
    assert_eq!(
        assess(archive, &before_identity, &affected)?.result,
        Some(Standing::Supported)
    );

    let identity = trial.identity(
        trial.local_calibration,
        trial.local_original,
        "execution",
        "complete",
        Status::Pass,
        8,
    )?;
    let head = trial.head()?;
    let corrected = assess(archive, &head, &trial.measured)?;
    assert_eq!(corrected.result, Some(Standing::Refuted));
    assert!(corrected.applicable_notices.contains(&trial.correction));
    let current = assess(archive, &head, &affected)?;
    assert_eq!(current.result, Some(Standing::Unresolved));
    assert!(current.applicable_notices.contains(&trial.correction));
    assert!(current.checks.iter().any(|check| {
        check.test == "delegated calibration applicability" && check.status == Status::Unknown
    }));
    assert_eq!(
        assess(archive, &head, &outside)?.result,
        Some(Standing::Supported)
    );
    assert_eq!(
        assess(archive, &before_identity, &affected)?.result,
        Some(Standing::Supported)
    );

    let revised = delegate(archive, &head, &trial.measured, Standing::Refuted)?;
    assert_eq!(
        assess(archive, &trial.head()?, &revised)?.result,
        Some(Standing::Refuted)
    );
    // A signed withdrawal of the link also invalidates a receipt that relied on it.
    let withdrawal = record(
        archive,
        Kind::Notice,
        LOCAL,
        json!({"action":"withdraw","reason":"corrected equipment register"}),
        vec![link("target", identity)],
    )?;
    fixture::sign(archive, Format::B, withdrawal, LOCAL, "notice", 9)?;
    assert_eq!(
        assess(archive, &trial.head()?, &revised)?.result,
        Some(Standing::Unresolved)
    );
    Ok(())
}
