use super::TrainingReview;
use crate::{
    profile::{Document, Kind},
    wire::Id,
};
use anyhow::{Context, Result, ensure};
use std::collections::BTreeSet;

pub(super) fn witnesses(
    review: &TrainingReview<'_>,
    receipt: &Document,
    arguments: &[Id],
) -> Result<BTreeSet<Id>> {
    let TrainingReview {
        store,
        before,
        request,
        aliases,
        trust,
        historical,
        access,
        ..
    } = review;
    let Some(plan) = request.forecast_plan else {
        return Ok(BTreeSet::new());
    };
    let (_, historical, _) = historical.as_ref().context("missing receipt inventory")?;
    let mut witnesses = BTreeSet::new();

    ensure!(
        store
            .read_for_training(before, plan, access)
            .is_ok_and(|d| d.kind == Kind::Plan),
        "registered forecast plan unavailable at feature cutoff"
    );
    let mut checked_outcomes = BTreeSet::new();
    for &id in arguments {
        let argument = store.read_for_training(historical, id, access)?;
        let witness_id = argument.one("release")?;
        let witness = store.read_for_training(historical, witness_id, access)?;
        ensure!(
            witness.kind == Kind::Occurrence
                && witness.subject == receipt.subject
                && witness.scope == receipt.scope
                && aliases.equivalent(witness.one("plan")?, plan)
                && witness.one("feature-cutoff")? == request.feature_cutoff
                && witness.field::<bool>("concealed")?
                && witness.field::<u64>("sealed")? < witness.field::<u64>("released")?,
            "missing controlled pre-outcome cutoff"
        );
        let signature = trust
            .signatures(aliases, witness_id, &witness.scope, "release")
            .into_iter()
            .find(|id| store.resolve_for_training(historical, *id, access).is_ok())
            .context("no admitted release signature at the receipt horizon")?;
        let mut outcomes: Vec<_> = argument.outcome_inputs().collect();
        ensure!(
            !outcomes.is_empty(),
            "argument has no declared outcome evidence"
        );
        while let Some(outcome) = outcomes.pop() {
            if !checked_outcomes.insert(outcome) {
                continue;
            }
            ensure!(
                !aliases.intersects(outcome, &before.objects),
                "outcome evidence visible at the feature cutoff"
            );
            ensure!(
                historical.objects.contains(&outcome),
                "declared outcome evidence unavailable at the receipt horizon"
            );
            if outcome.format().is_some() {
                let evidence = store.read_for_training(historical, outcome, access)?;
                outcomes.extend(evidence.outcome_inputs());
            }
        }
        witnesses.insert(signature);
    }
    Ok(witnesses)
}
