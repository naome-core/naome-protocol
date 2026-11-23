mod profiles;

use super::{EvidenceView, ReadInventory};
use crate::{
    migration::Equivalence,
    profile::{Document, Kind, Link},
    store::{Archive, Closure, Snapshot},
    wire::Id,
};
use anyhow::Result;
use profiles::Scope;
use std::collections::{BTreeMap, BTreeSet};

const ACTION: &str = "correct-input";

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CorrectionImpact {
    #[default]
    Outside,
    Open,
    Affected,
}

pub(crate) struct DependencyCorrection {
    source: BTreeSet<Id>,
    scope: Option<Scope>,
}

impl DependencyCorrection {
    pub(crate) fn is_correction(notice: &Document) -> bool {
        notice.kind == Kind::Notice
            && notice.fields.get("action").and_then(|value| value.as_str()) == Some(ACTION)
    }

    pub(crate) fn parse(
        notice: &Document,
        store: &Archive,
        snapshot: &Snapshot,
        aliases: &Equivalence,
        scope_invalid: bool,
    ) -> Result<Self> {
        anyhow::ensure!(Self::is_correction(notice), "expected an input correction");
        let mixed_notice = notice.roles("calibration").next().is_some();
        anyhow::ensure!(
            scope_invalid || !mixed_notice,
            "input correction cannot also target a calibration notice branch"
        );
        let source = aliases.endpoints(notice.one("source-version")?);
        let source_readable = source
            .iter()
            .any(|&id| store.readable(snapshot, id).is_ok());
        let evidence_readable = notice.one("evidence").is_ok_and(|id| {
            aliases
                .members(id)
                .any(|id| store.readable(snapshot, id).is_ok())
        });
        let scope = if !mixed_notice && source_readable && evidence_readable {
            notice.field("coverage").ok()
        } else {
            None
        };
        Ok(Self { source, scope })
    }

    fn source_version(&self, id: Id) -> bool {
        self.source.contains(&id)
    }

    fn dependency_edge(document: &Document, edge: &Link) -> bool {
        document.kind != Kind::Notice
            && matches!(
                document.dependency_role(edge),
                crate::profile::DependencyRole::Evidence | crate::profile::DependencyRole::Outcome
            )
    }

    fn edge_impact(&self, edge: &Link) -> Option<CorrectionImpact> {
        self.source_version(edge.target).then(|| {
            let input = (edge.role == "source-input")
                .then(|| serde_json::from_str(&edge.qualifier).ok())
                .flatten();
            match (&self.scope, input) {
                (Some(correction), Some(usage)) => correction.overlap(&usage),
                _ => CorrectionImpact::Open,
            }
        })
    }

    pub(crate) fn downstream(
        &self,
        store: &Archive,
        snapshot: &Snapshot,
        inventory: &ReadInventory,
        frontier: &mut BTreeSet<Id>,
    ) -> BTreeMap<Id, CorrectionImpact> {
        let mut impacts = BTreeMap::new();
        let mut dependents = BTreeMap::<Id, Vec<Id>>::new();
        let mut checked_blobs = BTreeSet::new();
        for (&id, document) in &inventory.documents {
            if self.source_version(id)
                || matches!(
                    document.kind,
                    Kind::Notice | Kind::Mapping | Kind::Attestation | Kind::Policy
                )
            {
                continue;
            }
            for edge in &document.inputs {
                if !Self::dependency_edge(document, edge) {
                    continue;
                }
                if (edge.target.format().is_some()
                    && !inventory.documents.contains_key(&edge.target))
                    || (edge.target.format().is_none()
                        && checked_blobs.insert(edge.target)
                        && store.readable(snapshot, edge.target).is_err())
                {
                    frontier.insert(edge.target);
                }
                if let Some(impact) = self.edge_impact(edge) {
                    let stored = impacts.entry(id).or_insert(impact);
                    *stored = (*stored).max(impact);
                } else {
                    dependents.entry(edge.target).or_default().push(id);
                }
            }
        }
        let mut pending: Vec<_> = impacts.keys().copied().collect();
        while let Some(parent) = pending.pop() {
            let impact = impacts[&parent];
            for &child in dependents.get(&parent).into_iter().flatten() {
                match impacts.entry(child) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(impact);
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry)
                        if *entry.get() < impact =>
                    {
                        entry.insert(impact);
                    }
                    _ => continue,
                }
                pending.push(child);
            }
        }
        impacts
    }

    pub(crate) fn exposure(
        &self,
        view: &EvidenceView<'_>,
        roots: &[Id],
        premises: &Closure,
    ) -> Result<CorrectionImpact> {
        if premises.frontier.is_empty() && self.source.is_disjoint(&premises.visited) {
            return Ok(CorrectionImpact::Outside);
        }
        let mut impact = CorrectionImpact::Outside;
        let mut seen = BTreeSet::new();
        let mut pending = roots.to_vec();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            if !premises.visited.contains(&id) {
                // An unreadable record might lead to the corrected source. A blob has no outgoing edges.
                if premises.frontier.contains_key(&id)
                    && (id.format().is_some() || self.source_version(id))
                {
                    impact = impact.max(CorrectionImpact::Open);
                }
                continue;
            }
            if self.source_version(id) {
                impact = impact.max(CorrectionImpact::Open);
            } else if id.format().is_some() {
                let read;
                let document = match view.inventory.documents.get(&id) {
                    Some(document) => document,
                    None => {
                        read = view.store.read(view.snapshot, id, "read")?;
                        &read
                    }
                };
                for edge in &document.inputs {
                    if !Self::dependency_edge(document, edge) {
                        continue;
                    }
                    if let Some(direct) = self.edge_impact(edge) {
                        impact = impact.max(direct);
                    } else {
                        pending.push(edge.target);
                    }
                }
            }
            if impact == CorrectionImpact::Affected {
                break;
            }
        }
        Ok(impact)
    }
}

pub(crate) type CorrectionKeys = BTreeMap<String, BTreeMap<Id, BTreeSet<String>>>;

pub(crate) fn correction_keys(policy: &Document) -> Result<CorrectionKeys> {
    match policy.fields.get("input_correction_keys") {
        Some(_) => policy.field("input_correction_keys"),
        None => Ok(BTreeMap::new()),
    }
}

pub(crate) fn correction_signature(
    view: &EvidenceView<'_>,
    keys: &CorrectionKeys,
    id: Id,
    notice: &Document,
) -> Option<Id> {
    let versions = keys.get(&notice.scope)?;
    view.aliases
        .members(notice.one("source-version").ok()?)
        .find_map(|version| {
            view.signature(
                id,
                &notice.scope,
                "input-correction",
                versions.get(&version)?,
            )
        })
}

pub(crate) struct AdmittedCorrection {
    pub id: Id,
    pub signature: Id,
    pub correction: DependencyCorrection,
}

pub(crate) fn admitted_corrections(
    view: &EvidenceView<'_>,
    keys: &CorrectionKeys,
    scope_invalid: bool,
) -> Result<Vec<AdmittedCorrection>> {
    let mut admitted = Vec::new();
    if !keys.is_empty() {
        for &id in &view.inventory.notices {
            let notice = &view.inventory.documents[&id];
            if !DependencyCorrection::is_correction(notice) {
                continue;
            }
            if let Some(signature) = correction_signature(view, keys, id, notice) {
                admitted.push(AdmittedCorrection {
                    id,
                    signature,
                    correction: DependencyCorrection::parse(
                        notice,
                        view.store,
                        view.snapshot,
                        view.aliases,
                        scope_invalid,
                    )?,
                });
            }
        }
    }
    Ok(admitted)
}
