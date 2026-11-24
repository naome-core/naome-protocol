use super::{EVALUATOR_ID, Standing, assess_for_evaluator};
use crate::{
    fixture,
    profile::{Kind, PUBLIC, link},
    store::Archive,
    wire::{Format, Id},
};
use anyhow::Result;
use serde_json::json;

#[test]
fn archived_and_current_rules_apply_their_own_manifest_encoding() -> Result<()> {
    for canonical in [false, true] {
        let archive = Archive::memory()?;
        let case = fixture::seed(&archive, Format::A)?;
        let west = archive.snapshot("West", Default::default())?;
        let bytes = if canonical {
            west.manifest()?.bytes()?
        } else {
            serde_json::to_vec_pretty(west.manifest()?)?
        };
        let id = archive.blob(&bytes, Id::blob(PUBLIC))?;
        let witness = fixture::put(
            &archive,
            Format::A,
            Kind::Occurrence,
            "West",
            "notice-horizon",
            json!({"time": 99}),
            vec![link("inventory", id)],
        )?;
        fixture::sign(&archive, Format::A, witness, "notice-horizon", "horizon", 5)?;
        let head = archive.snapshot("authority review", Default::default())?;
        let mut query = case.queries[2].clone();
        query.current_at = Some(100);
        let old = assess_for_evaluator(&archive, &head, &query, "rep-rust-rules-5")?;
        assert!(old.horizon_complete);
        assert_eq!(old.result, Some(Standing::Supported));
        let new = assess_for_evaluator(&archive, &head, &query, EVALUATOR_ID)?;
        assert_eq!(new.horizon_complete, canonical);
        if canonical {
            assert_eq!(serde_json::to_value(&new)?, serde_json::to_value(&old)?);
        } else {
            assert_eq!(new.result, Some(Standing::Unresolved));
            assert!(
                new.obligations
                    .contains("required notice authority unavailable or stale: West")
            );
        }
    }
    Ok(())
}

#[test]
fn record_budget_suffix_keeps_the_previous_receipt_replayable() -> Result<()> {
    let a = Archive::memory()?;
    let case = fixture::seed(&a, Format::A)?;
    let s = a.head(case.s0)?;
    let mut q = case.queries[0].clone();
    q.budget = 2;
    let old = "rep-rust-rules-6";
    let transcript = assess_for_evaluator(&a, &s, &q, old)?;
    let id = super::deposit_receipt_for_evaluator(&a, &s, &q, &transcript, Format::A, old)?;
    // Exact receipt returned by the preceding packaged executable, commit 3aabddd.
    assert_eq!(
        id.to_string(),
        "1:0:afe0fa7e184a35af4c475283aa77d38076b6d9df6e05c4edf8d46475261bb17b"
    );
    let new = assess_for_evaluator(&a, &s, &q, EVALUATOR_ID)?;
    assert_eq!(new.result, transcript.result);
    assert!(!new.horizon_complete);
    let from = new.unscanned_records_from.unwrap();
    assert_eq!(new.frontier, [from].into());
    assert_eq!(
        s.records()
            .filter(|id| *id >= from)
            .collect::<std::collections::BTreeSet<_>>(),
        transcript.frontier
    );
    Ok(())
}
