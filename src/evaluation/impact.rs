use super::{
    EvidenceView, ReadInventory,
    calibration_identity::CalibrationIdentities,
    correction::{MeasurementUse, Notice, WithdrawalRules},
    dependency_correction::{
        CorrectionImpact, DependencyCorrection, correction_keys, correction_signature,
    },
    inventory::UnscannedRecords,
};
use crate::{
    migration::Equivalence,
    profile::{CALIBRATION_EVENT_IDENTITY, Document, Kind},
    store::{Archive, Snapshot},
    wire::Id,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

impl Archive {
    pub fn impact(
        &self,
        s: &Snapshot,
        notice: Id,
        mappings: &[Id],
        budget: usize,
    ) -> Result<Impact> {
        self.impact_with_policy(s, notice, mappings, budget, None)
    }

    /// Trace a correction. An explicit recipient policy admits physical identity
    /// links for this snapshot in addition to the format mappings.
    pub fn impact_with_policy(
        &self,
        s: &Snapshot,
        notice: Id,
        mappings: &[Id],
        budget: usize,
        policy: Option<Id>,
    ) -> Result<Impact> {
        self.impact_report(s, notice, mappings, budget, policy, false)
            .map(|(impact, _)| impact)
    }

    /// Report a bounded impact scan without individually listing the unscanned record suffix.
    pub fn impact_compact(
        &self,
        s: &Snapshot,
        notice: Id,
        mappings: &[Id],
        budget: usize,
        policy: Option<Id>,
    ) -> Result<Value> {
        let (impact, unscanned) = self.impact_report(s, notice, mappings, budget, policy, true)?;
        let mut report = serde_json::to_value(impact)?;
        report["snapshot"] = serde_json::to_value(s.id()?)?;
        report["unscanned"] = serde_json::to_value(unscanned)?;
        Ok(report)
    }

    fn impact_report(
        &self,
        s: &Snapshot,
        notice: Id,
        mappings: &[Id],
        budget: usize,
        policy: Option<Id>,
        compact: bool,
    ) -> Result<(Impact, Option<UnscannedRecords>)> {
        let n = self.read(s, notice, "read")?;
        ensure!(n.kind == Kind::Notice, "expected notice");
        let mut eq = Equivalence::default();
        for &m in mappings {
            eq.add(self.verify_mapping(s, m)?);
        }
        let correction_notice = Notice(&n);
        let input_correction = DependencyCorrection::is_correction(&n)
            .then(|| DependencyCorrection::parse(&n, self, s, &eq, true))
            .transpose()?;
        let calibration = input_correction.is_none() && n.roles("calibration").next().is_some();
        let (initial, correction) = if calibration {
            (
                eq.endpoints(n.one("calibration")?),
                Some(correction_notice.calibration_limits()?),
            )
        } else if input_correction.is_some() {
            (BTreeSet::new(), None)
        } else {
            let targets = correction_notice.target_endpoints(&eq);
            ensure!(!targets.is_empty(), "missing notice target");
            (targets, None)
        };
        let withdrawal =
            !calibration && n.fields.get("action").and_then(Value::as_str) == Some("withdraw");
        let (inventory, mut frontier, unscanned) =
            ReadInventory::bounded(self, s, "read", budget, None, |_| true);
        if !compact && let Some(tail) = unscanned {
            tail.expand(s, &mut frontier);
        }
        let docs = &inventory.documents;
        let mut foreign_uses = BTreeSet::new();
        let mut correction_authority = None;
        if let Some(policy_id) = policy
            && unscanned.is_none()
            && frontier.is_empty()
        {
            let policy = self.read(s, policy_id, "read")?;
            ensure!(
                policy.kind == Kind::Policy && policy.scope == "research-policy-1",
                "unsupported impact policy"
            );
            let evidence = EvidenceView {
                store: self,
                snapshot: s,
                inventory: &inventory,
                aliases: &eq,
            };
            if input_correction.is_some() {
                correction_authority =
                    correction_signature(&evidence, &correction_keys(&policy)?, notice, &n);
            } else {
                let identities = CalibrationIdentities::admit(&evidence, &policy, budget)?;
                foreign_uses = identities.affected_uses(
                    &evidence,
                    notice,
                    &n,
                    docs.keys().copied(),
                    budget,
                    &mut frontier,
                )?;
            }
        }
        if let Some(correction) = input_correction {
            let impacts = correction.downstream(self, s, &inventory, &mut frontier);
            let matching = |status| {
                impacts
                    .iter()
                    .filter_map(move |(id, &impact)| (impact == status).then_some(id))
            };
            return Ok((
                Impact {
                    notice,
                    by_kind: group_impact(docs, matching(CorrectionImpact::Affected))?,
                    needs_reassessment: group_impact(docs, matching(CorrectionImpact::Open))?,
                    outside_scope: if unscanned.is_none() && frontier.is_empty() {
                        group_impact(docs, matching(CorrectionImpact::Outside))?
                    } else {
                        BTreeMap::new()
                    },
                    correction_authority,
                    frontier,
                },
                unscanned,
            ));
        }
        let argument_excluded = |id: Id, argument: &Document| {
            !foreign_uses.contains(&id)
                && argument.kind == Kind::Argument
                && MeasurementUse::for_argument(
                    argument,
                    |id| docs.get(&id),
                    WithdrawalRules::Scoped,
                )
                .is_some_and(|measurement| {
                    correction_notice.excludes_current_withdrawal(&argument.scope, measurement)
                })
        };
        let mut dependents = BTreeMap::<Id, Vec<Id>>::new();
        let mut excluded_uses = BTreeSet::new();
        let mut direct = BTreeSet::new();
        for (&id, d) in docs {
            if matches!(
                d.kind,
                Kind::Notice | Kind::Mapping | Kind::Attestation | Kind::Policy
            ) {
                continue;
            }
            // A different differential bias can change scores and bounds while arithmetic replay passes.
            if d.kind == Kind::Label
                && d.fields.get("check").and_then(Value::as_str) == Some("arithmetic replay")
                && n.field::<i64>("bias").is_ok()
            {
                continue;
            }
            if withdrawal {
                let excluded = match d.kind {
                    Kind::Argument => argument_excluded(id, d),
                    Kind::Occurrence if d.fields.contains_key("gauge") => correction_notice
                        .excludes_current_withdrawal(
                            &d.scope,
                            MeasurementUse::from_reading(d, WithdrawalRules::Scoped),
                        ),
                    Kind::Assessment => {
                        let mut arguments = d.roles("argument").peekable();
                        arguments.peek().is_some()
                            && arguments.all(|id| {
                                docs.get(&id).is_some_and(|doc| argument_excluded(id, doc))
                            })
                            && !d.inputs.iter().any(|edge| {
                                edge.role != "dependency" && initial.contains(&edge.target)
                            })
                    }
                    _ => false,
                };
                if excluded {
                    excluded_uses.insert(id);
                }
            }
            // Claims and arguments can themselves be reported before any
            // assessment or other record links them to a downstream use.
            if !calibration
                && initial.contains(&id)
                && matches!(d.kind, Kind::Argument | Kind::Claim)
                && correction_notice.in_scope(&d.scope)
                && !excluded_uses.contains(&id)
            {
                direct.insert(id);
            }
            for edge in &d.inputs {
                if matches!(
                    edge.role.as_str(),
                    "source-calibration" | "recipient-calibration"
                ) && d.kind == Kind::Claim
                    && d.fields.get("predicate").and_then(Value::as_str)
                        == Some(CALIBRATION_EVENT_IDENTITY)
                {
                    continue;
                }
                if initial.contains(&edge.target)
                    && correction.as_ref().is_some_and(|limits| limits.excludes(d))
                {
                    continue;
                }
                dependents.entry(edge.target).or_default().push(id);
            }
        }
        let mut reached_without_impact = BTreeSet::new();
        let mut affected = initial
            .union(&foreign_uses)
            .copied()
            .collect::<BTreeSet<_>>();
        let mut pending: Vec<_> = affected.iter().copied().collect();
        while let Some(id) = pending.pop() {
            let propagates = affected.contains(&id);
            if let Some(children) = dependents.get(&id) {
                for &child in children {
                    // An unaffected use can still carry premises into another
                    // argument, which evaluates them in its own context.
                    let newly_reached = if !excluded_uses.contains(&child)
                        && (propagates || docs[&child].kind == Kind::Argument)
                    {
                        affected.insert(child)
                    } else {
                        !affected.contains(&child) && reached_without_impact.insert(child)
                    };
                    if newly_reached {
                        pending.push(child);
                    }
                }
            }
        }
        Ok((
            Impact {
                notice,
                by_kind: group_impact(docs, affected.difference(&initial).chain(&direct))?,
                needs_reassessment: BTreeMap::new(),
                outside_scope: BTreeMap::new(),
                correction_authority: None,
                frontier,
            },
            unscanned,
        ))
    }
}

fn group_impact<'a>(
    docs: &BTreeMap<Id, Document>,
    records: impl Iterator<Item = &'a Id>,
) -> Result<BTreeMap<String, BTreeSet<Id>>> {
    let mut groups: BTreeMap<String, BTreeSet<Id>> = BTreeMap::new();
    for id in records {
        if let Some(d) = docs.get(id) {
            groups
                .entry(
                    serde_json::to_value(d.kind)?
                        .as_str()
                        .context("kind")?
                        .into(),
                )
                .or_default()
                .insert(*id);
        }
    }
    Ok(groups)
}

#[derive(Serialize)]
pub struct Impact {
    pub notice: Id,
    pub by_kind: BTreeMap<String, BTreeSet<Id>>,
    /// Possible uses whose declared input coverage cannot exclude the change.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub needs_reassessment: BTreeMap<String, BTreeSet<Id>>,
    /// Recorded uses with disjoint selectors and a complete search frontier.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub outside_scope: BTreeMap<String, BTreeSet<Id>>,
    /// Admitted signer for an input correction. Without it the impact trace is advisory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correction_authority: Option<Id>,
    pub frontier: BTreeSet<Id>,
}
