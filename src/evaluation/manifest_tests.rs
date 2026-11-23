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
