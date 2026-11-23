//! Admission of physical identity assertions, separate from format equivalence.
use super::{
    Status,
    correction::{
        LinkedCalibrationNotice, MeasurementUse, WithdrawalApplicability, WithdrawalRules,
    },
    inventory::EvidenceView,
};
use crate::{
    migration::Equivalence,
    profile::{CALIBRATION_EVENT_IDENTITY, Document, Kind},
    store::ManifestEncoding,
    wire::Id,
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

type Keys = BTreeMap<String, BTreeSet<String>>;

impl EvidenceView<'_> {
    fn calibration(&self, id: Id) -> Result<&Document> {
        let doc = self
            .inventory
            .documents
            .get(&id)
            .context("unavailable calibration endpoint")?;
        ensure!(
            doc.kind == Kind::Calibration,
            "wrong calibration endpoint kind"
        );
        Ok(doc)
    }

    fn check_inventory(&self, head: Id, calibration: Id, original: Id) -> Result<String> {
        let manifest =
            self.store
                .linked_snapshot(self.snapshot, head, ManifestEncoding::Canonical)?;
        ensure!(
            manifest.objects.contains(&calibration) && manifest.objects.contains(&original),
            "calibration or original outside its asserted inventory"
        );
        self.store.resolve(self.snapshot, original, "read")?;
        Ok(manifest.source)
    }
}

#[derive(Default)]
pub(crate) struct CalibrationIdentities {
    // Keyed by the format-equivalence representative of the foreign calibration.
    by_source: BTreeMap<Id, BTreeMap<String, BTreeMap<Id, IdentityStanding>>>,
    foreign_keys: Keys,
}

#[derive(Default)]
struct IdentityStanding {
    support: Option<AdmittedIdentity>,
    conflict: bool,
    pending: bool,
}

pub(crate) struct AdmittedIdentity {
    pub source_scope: String,
    pub recipient_scope: String,
    pub mode: String,
    pub inputs: BTreeSet<Id>,
}

impl CalibrationIdentities {
    pub(crate) fn admit(
        evidence: &EvidenceView<'_>,
        policy: &Document,
        budget: usize,
    ) -> Result<Self> {
        let Some(foreign_keys) = policy.fields.get("foreign_calibration_keys") else {
            return Ok(Self::default());
        };
        let foreign_keys: Keys = serde_json::from_value(foreign_keys.clone())?;
        let keys: Keys = policy.field("keys")?;
        let mut result = Self {
            foreign_keys,
            ..Self::default()
        };
        let Some(identity_keys) = keys.get("calibration-identity") else {
            return Ok(result);
        };
        for &check in evidence.inventory.identity_checks()? {
            let assertion = &evidence.inventory.documents[&check];
            let Some(signature) = evidence.signature(
                check,
                &assertion.scope,
                "calibration-identity",
                identity_keys,
            ) else {
                continue;
            };
            let Ok((claim, source, recipient)) = endpoints(evidence, assertion) else {
                continue;
            };
            let Ok(from) = evidence.calibration(source) else {
                continue;
            };
            if !result.foreign_keys.contains_key(&from.scope) {
                continue;
            }
            let to = evidence.calibration(recipient);
            if to.as_ref().is_ok_and(|to| to.scope != assertion.scope)
                || evidence.aliases.representative(source)
                    == evidence.aliases.representative(recipient)
            {
                continue;
            }
            let slot = result
                .by_source
                .entry(evidence.aliases.representative(source))
                .or_default()
                .entry(assertion.scope.clone())
                .or_default()
                .entry(evidence.aliases.representative(recipient))
                .or_default();
            let scope = [&from.scope, &assertion.scope];
            let endpoints = [source, recipient];
            let declaration = [check, signature, assertion.one("claim")?].into();
            if challenged(evidence, scope, endpoints, &declaration, &keys) {
                continue;
            }
            match assertion.field::<Status>("status") {
                Ok(Status::Fail) => slot.conflict = true,
                Ok(Status::Unknown) => slot.pending = true,
                Ok(Status::Pass) => {
                    match verify_check(evidence, check, signature, claim, assertion, budget) {
                        Ok(proof)
                            if !challenged(evidence, scope, endpoints, &proof.inputs, &keys) =>
                        {
                            if slot.support.is_none() {
                                slot.support = Some(proof);
                            }
                        }
                        _ => slot.pending = true,
                    }
                }
                _ => {}
            }
        }
        Ok(result)
    }

    pub(crate) fn recipients_for(
        &self,
        aliases: &Equivalence,
        source: Id,
    ) -> impl Iterator<Item = (Id, &AdmittedIdentity)> {
        self.by_source
            .get(&aliases.representative(source))
            .into_iter()
            .flat_map(BTreeMap::values)
            .filter_map(|targets| {
                let mut admitted = targets.iter().filter_map(|(&id, standing)| {
                    standing.support.as_ref().map(|proof| (id, proof))
                });
                let selected = admitted.next()?;
                // Simultaneous competing assertions in one recipient scope require adjudication.
                (admitted.next().is_none()
                    && !targets[&selected.0].conflict
                    && !targets.iter().any(|(&id, candidate)| {
                        id != selected.0 && candidate.pending && !candidate.conflict
                    }))
                .then_some(selected)
            })
    }

    pub(crate) fn affected_uses(
        &self,
        evidence: &EvidenceView<'_>,
        notice_id: Id,
        notice: &Document,
        candidates: impl IntoIterator<Item = Id>,
        budget: usize,
        frontier: &mut BTreeSet<Id>,
    ) -> Result<BTreeSet<Id>> {
        let mut foreign_uses = BTreeSet::new();
        let Some(linked) = LinkedCalibrationNotice::parse(notice) else {
            return Ok(foreign_uses);
        };
        let Some(signature) =
            self.correction_signature(evidence, notice_id, &notice.scope, linked.signature_role())
        else {
            return Ok(foreign_uses);
        };
        let recipients: BTreeMap<_, _> = self
            .recipients_for(evidence.aliases, linked.source)
            .filter(|(_, proof)| proof.source_scope == notice.scope)
            .flat_map(|(recipient, proof)| {
                evidence
                    .aliases
                    .members(recipient)
                    .map(move |id| (id, proof))
            })
            .collect();
        if recipients.is_empty() {
            return Ok(foreign_uses);
        }
        let docs = &evidence.inventory.documents;
        let mut unknown_uses = BTreeSet::new();
        for id in candidates {
            let Some(document) = docs.get(&id) else {
                continue;
            };
            let Ok(calibration) = document.one("calibration") else {
                continue;
            };
            let Some(proof) = recipients.get(&calibration) else {
                continue;
            };
            if document.scope != proof.recipient_scope {
                continue;
            }
            let measurement = match document.kind {
                Kind::Argument
                    if document.fields.get("method").and_then(Value::as_str)
                        == Some("simultaneous-bounds") =>
                {
                    MeasurementUse::for_argument(
                        document,
                        |reading| docs.get(&reading),
                        WithdrawalRules::Scoped,
                    )
                }
                Kind::Occurrence if document.fields.contains_key("gauge") => Some(
                    MeasurementUse::from_reading(document, WithdrawalRules::Scoped),
                ),
                _ => None,
            };
            if let Some(measurement) = measurement {
                if !measurement.complete() {
                    unknown_uses.insert(id);
                } else if measurement.matches_mode(&proof.mode) {
                    match linked.applicability(measurement) {
                        WithdrawalApplicability::Applies => {
                            foreign_uses.insert(id);
                        }
                        WithdrawalApplicability::Unknown(_) => {
                            unknown_uses.insert(id);
                        }
                        WithdrawalApplicability::Excluded => {}
                    }
                }
            }
        }
        if foreign_uses.is_empty() && unknown_uses.is_empty() {
            return Ok(foreign_uses);
        }
        linked.verify_evidence(|id| docs.get(&id).context("notice evidence unavailable"))?;
        let dependencies = evidence.store.explain(
            evidence.snapshot,
            &[notice_id, signature],
            "read",
            budget,
            48,
        );
        if !dependencies.frontier.is_empty() {
            foreign_uses.clear();
        }
        frontier.extend(dependencies.frontier.keys());
        frontier.extend(unknown_uses);
        Ok(foreign_uses)
    }

    pub(crate) fn correction_signature(
        &self,
        evidence: &EvidenceView<'_>,
        notice: Id,
        scope: &str,
        role: &str,
    ) -> Option<Id> {
        let keys = self.foreign_keys.get(scope)?;
        evidence.signature(notice, scope, role, keys)
    }
}

fn endpoints<'a>(
    view: &'a EvidenceView<'_>,
    assertion: &Document,
) -> Result<(&'a Document, Id, Id)> {
    let claim = view
        .inventory
        .documents
        .get(&assertion.one("claim")?)
        .context("identity claim unavailable")?;
    ensure!(
        claim.kind == Kind::Claim
            && claim.field::<String>("predicate")? == CALIBRATION_EVENT_IDENTITY
            && claim.field::<String>("granularity")? == "execution"
            && claim.scope == assertion.scope
            && claim.subject == assertion.subject,
        "identity claim is outside the correction profile"
    );
    Ok((
        claim,
        claim.one("source-calibration")?,
        claim.one("recipient-calibration")?,
    ))
}

fn verify_check(
    view: &EvidenceView<'_>,
    check: Id,
    signature: Id,
    claim: &Document,
    assertion: &Document,
    budget: usize,
) -> Result<AdmittedIdentity> {
    ensure!(
        assertion.field::<String>("coverage")? == "complete"
            && !assertion.field::<String>("basis")?.trim().is_empty(),
        "identity check lacks a reason or a closed search scope"
    );
    let source = claim.one("source-calibration")?;
    let recipient = claim.one("recipient-calibration")?;
    let from = view.calibration(source)?;
    let to = view.calibration(recipient)?;
    let mode: String = from.field("mode")?;
    let time_basis: String = from.field("event_time_basis")?;
    ensure!(
        !mode.is_empty()
            && !time_basis.is_empty()
            && to.field::<String>("event_time_basis")? == time_basis
            && to.field::<String>("mode")? == mode
            && [from, to].iter().all(|record| {
                record
                    .fields
                    .get("unit")
                    .is_none_or(|unit| unit.as_str() == Some("um"))
            }),
        "calibration modes, time axes or bias units are incompatible"
    );
    let source_original = assertion.one("source-evidence")?;
    let recipient_original = assertion.one("recipient-evidence")?;
    ensure!(
        source_original != recipient_original
            && ![source, recipient].contains(&source_original)
            && ![source, recipient].contains(&recipient_original),
        "distinct original evidence is required"
    );
    let source_inventory =
        view.check_inventory(assertion.one("source-inventory")?, source, source_original)?;
    let recipient_inventory = view.check_inventory(
        assertion.one("recipient-inventory")?,
        recipient,
        recipient_original,
    )?;
    ensure!(
        source_inventory != recipient_inventory,
        "expected two archive sources"
    );
    let proof = view
        .store
        .explain(view.snapshot, &[check, signature], "read", budget, 48);
    ensure!(
        proof.frontier.is_empty(),
        "identity has an unavailable dependency"
    );
    Ok(AdmittedIdentity {
        source_scope: from.scope.clone(),
        recipient_scope: to.scope.clone(),
        mode,
        inputs: proof.visited,
    })
}

fn challenged(
    view: &EvidenceView<'_>,
    scopes: [&String; 2],
    endpoints: [Id; 2],
    inputs: &BTreeSet<Id>,
    keys: &Keys,
) -> bool {
    let Some(trusted) = keys.get("notice") else {
        return false;
    };
    view.inventory.notices.iter().any(|&id| {
        let notice = &view.inventory.documents[&id];
        scopes.contains(&&notice.scope)
            && matches!(
                notice
                    .fields
                    .get("action")
                    .and_then(serde_json::Value::as_str),
                Some("withdraw" | "challenge" | "replace")
            )
            && notice.roles("target").any(|target| {
                let targets = view.aliases.endpoints(target);
                !endpoints.iter().any(|id| targets.contains(id)) && !targets.is_disjoint(inputs)
            })
            && view
                .signature(id, &notice.scope, "notice", trusted)
                .is_some()
    })
}
