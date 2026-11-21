#![cfg(feature = "cli")]

use anyhow::{Context, Result};
use naome_protocol::{
    evaluation::{Query, Standing, assess},
    fixture::{self, Investigation, put},
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
