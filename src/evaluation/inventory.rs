use super::{Query, Receipt};
use crate::{
    migration::Equivalence,
    profile::{CALIBRATION_EVENT_IDENTITY, Document, Kind, verify_attestation},
    store::{Archive, Snapshot},
    wire::Id,
};
use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Readable records and their indexes for one snapshot in the current archive transaction.
#[derive(Default)]
pub(crate) struct ReadInventory {
    pub(crate) documents: BTreeMap<Id, Document>,
    pub(crate) notices: BTreeSet<Id>,
    inspected: BTreeMap<Id, bool>,
    remaining_cache_bytes: Option<usize>,
    attestations: BTreeMap<Id, BTreeSet<Id>>,
    identity_checks: BTreeSet<Id>,
    skipped_identity_checks: bool,
    sizes: BTreeMap<Id, usize>,
    complete_scan: Option<(usize, usize)>,
}

/// A suffix of the record order in the report's immutable snapshot.
#[derive(Clone, Copy, Serialize)]
pub(super) struct UnscannedRecords {
    after: Option<Id>,
    next: Id,
}

impl ReadInventory {
    pub(crate) fn for_replay() -> Self {
        Self {
            remaining_cache_bytes: Some(super::default_byte_limit()),
            ..Self::default()
        }
    }

    pub(crate) fn load(
        store: &Archive,
        snapshot: &Snapshot,
        retain_for_replay: bool,
    ) -> Result<Self> {
        let mut inventory = Self::for_replay();
        for id in snapshot.records() {
            let retain = if retain_for_replay {
                let size = inventory.size(store, id)?;
                inventory.reserve_cache_bytes(size)
            } else {
                false
            };
            inventory.read(store, snapshot, id, retain);
        }
        Ok(inventory)
    }

    pub(super) fn bounded(
        store: &Archive,
        snapshot: &Snapshot,
        purpose: &str,
        budget: usize,
        mut after: Option<Id>,
        retain: impl Fn(&Document) -> bool,
    ) -> (Self, BTreeSet<Id>, Option<UnscannedRecords>) {
        let mut records = snapshot.records_after(after);
        let mut inventory = Self::default();
        let mut frontier = BTreeSet::new();
        for id in records.by_ref().take(budget) {
            after = Some(id);
            if !inventory.read_for(store, snapshot, id, purpose, &retain) {
                frontier.insert(id);
            }
        }
        let unscanned = records.next().map(|next| UnscannedRecords { after, next });
        (inventory, frontier, unscanned)
    }

    pub(crate) fn notice_search_complete(&self) -> bool {
        !self.inspected.values().any(|&hidden| hidden)
    }

    fn size(&mut self, store: &Archive, id: Id) -> Result<usize> {
        if let Some(&size) = self.sizes.get(&id) {
            return Ok(size);
        }
        let size = store.size(id)?;
        self.sizes.insert(id, size);
        Ok(size)
    }

    fn reserve_cache_bytes(&mut self, size: usize) -> bool {
        let Some(remaining) = &mut self.remaining_cache_bytes else {
            return true;
        };
        if size > *remaining {
            return false;
        }
        *remaining -= size;
        true
    }

    pub(crate) fn attestations(&self, target: Id) -> impl Iterator<Item = (Id, &Document)> {
        self.attestations
            .get(&target)
            .into_iter()
            .flatten()
            .map(|&id| (id, &self.documents[&id]))
    }

    pub(crate) fn identity_checks(&self) -> Result<&BTreeSet<Id>> {
        ensure!(
            !self.skipped_identity_checks,
            "identity check retention budget exhausted"
        );
        Ok(&self.identity_checks)
    }

    pub(crate) fn read(
        &mut self,
        store: &Archive,
        snapshot: &Snapshot,
        id: Id,
        retain: bool,
    ) -> bool {
        self.read_for(store, snapshot, id, "read", |_| retain)
    }

    fn read_for(
        &mut self,
        store: &Archive,
        snapshot: &Snapshot,
        id: Id,
        purpose: &str,
        retain: impl FnOnce(&Document) -> bool,
    ) -> bool {
        let hidden = match store.read(snapshot, id, purpose) {
            Ok(document) => {
                let retain = retain(&document);
                match document.kind {
                    Kind::Notice => {
                        self.notices.insert(id);
                    }
                    Kind::Attestation => {
                        if let Ok(target) = document.one("signed-original") {
                            self.attestations.entry(target).or_default().insert(id);
                        }
                    }
                    Kind::Assessment
                        if document
                            .fields
                            .get("test")
                            .and_then(serde_json::Value::as_str)
                            == Some(CALIBRATION_EVENT_IDENTITY) =>
                    {
                        if !retain {
                            self.skipped_identity_checks = true;
                            self.inspected.insert(id, false);
                            return true;
                        }
                        self.identity_checks.insert(id);
                    }
                    _ if !retain => {
                        self.inspected.insert(id, false);
                        return true;
                    }
                    _ => {}
                }
                self.documents.insert(id, document);
                self.inspected.insert(id, false);
                return true;
            }
            Err(_) => store.may_be_notice(snapshot, id, purpose),
        };
        self.inspected.insert(id, hidden);
        false
    }

    pub(super) fn search(
        &mut self,
        store: &Archive,
        snapshot: &Snapshot,
        query: &Query,
        out: &mut Receipt,
        records_only: bool,
    ) -> Result<()> {
        if let Some((records, bytes)) = self.complete_scan
            && (if records_only {
                records
            } else {
                snapshot.objects.len()
            }) <= query.budget
            && bytes <= query.byte_limit
        {
            out.visible_notices.extend(&self.notices);
            return Ok(());
        }

        let mut hidden = false;
        let mut bytes_left = query.byte_limit;
        let mut records = 0;
        let mut all_objects = snapshot.objects.iter().copied();
        let mut records_in_order = snapshot.records();
        let objects: &mut dyn Iterator<Item = Id> = if records_only {
            &mut records_in_order
        } else {
            &mut all_objects
        };
        for (index, id) in objects.enumerate() {
            if index >= query.budget {
                out.frontier.insert(id);
                continue;
            }
            if id.format().is_none() {
                continue;
            }
            records += 1;
            let size = self.size(store, id)?;
            if size > bytes_left {
                out.frontier.insert(id);
                continue;
            }
            bytes_left -= size;
            if !self.inspected.contains_key(&id) {
                let retain = self.reserve_cache_bytes(size);
                self.read(store, snapshot, id, retain);
            }
            if self.notices.contains(&id) {
                out.visible_notices.insert(id);
            }
            if self.inspected.get(&id) == Some(&true) {
                out.frontier.insert(id);
                hidden = true;
            }
        }

        ensure!(
            !hidden,
            "notice horizon contains inaccessible or opaque records"
        );
        ensure!(out.frontier.is_empty(), "notice search budget exhausted");
        self.complete_scan = Some((records, query.byte_limit - bytes_left));
        Ok(())
    }
}

impl UnscannedRecords {
    pub(super) fn expand(self, snapshot: &Snapshot, frontier: &mut BTreeSet<Id>) {
        frontier.extend(snapshot.records_after(self.after));
    }
}

enum GraphScan {
    Legacy,
    Compact { after: Option<Id> },
}

impl Archive {
    /// Include arguments citing the target and candidate defeaters in the declared inventory.
    pub fn argument_graph(
        &self,
        snapshot: &Snapshot,
        target: Id,
        purpose: &str,
        budget: usize,
        depth: usize,
    ) -> Result<Value> {
        self.argument_graph_report(snapshot, target, purpose, budget, depth, GraphScan::Legacy)
    }

    /// Use a compact record frontier. The returned cursor can resume the scan at the same snapshot.
    pub fn argument_graph_page(
        &self,
        snapshot: &Snapshot,
        target: Id,
        purpose: &str,
        budget: usize,
        depth: usize,
        after: Option<Id>,
    ) -> Result<Value> {
        ensure!(
            after.is_none_or(|id| id.format().is_some() && snapshot.objects.contains(&id)),
            "scan cursor is outside the snapshot record inventory"
        );
        self.argument_graph_report(
            snapshot,
            target,
            purpose,
            budget,
            depth,
            GraphScan::Compact { after },
        )
    }

    fn argument_graph_report(
        &self,
        snapshot: &Snapshot,
        target: Id,
        purpose: &str,
        budget: usize,
        depth: usize,
        scan: GraphScan,
    ) -> Result<Value> {
        let after = match scan {
            GraphScan::Legacy => None,
            GraphScan::Compact { after } => after,
        };
        let keep_argument = |doc: &Document| {
            doc.kind == Kind::Argument && doc.roles("claim").any(|id| id == target)
        };
        let (inventory, mut unsearched, unscanned) =
            ReadInventory::bounded(self, snapshot, purpose, budget, after, keep_argument);
        if matches!(scan, GraphScan::Legacy)
            && let Some(tail) = unscanned
        {
            tail.expand(snapshot, &mut unsearched);
        }
        let arguments: Vec<_> = inventory
            .documents
            .iter()
            .filter(|(_, doc)| doc.kind == Kind::Argument)
            .map(|(&id, _)| id)
            .collect();
        let mut roots = arguments.clone();
        roots.push(target);
        let graph = self.explain(snapshot, &roots, purpose, budget, depth);
        let contrary: Vec<_> = inventory
            .notices
            .iter()
            .filter(|id| {
                inventory.documents[id]
                    .inputs
                    .iter()
                    .any(|edge| graph.visited.contains(&edge.target))
            })
            .collect();
        let mut report = json!({
            "snapshot": snapshot.id()?,
            "source": snapshot.source,
            "target": target,
            "arguments": arguments,
            "dependencies": graph,
            "candidate_notices": contrary,
            "unsearched": unsearched,
            "budget_per_phase": budget,
        });
        if matches!(scan, GraphScan::Compact { .. }) {
            report["unscanned"] = json!(unscanned);
        }
        Ok(report)
    }
}

pub(crate) struct EvidenceView<'a> {
    pub store: &'a Archive,
    pub snapshot: &'a Snapshot,
    pub inventory: &'a ReadInventory,
    pub aliases: &'a Equivalence,
}

impl EvidenceView<'_> {
    pub(crate) fn signature(
        &self,
        id: Id,
        scope: &str,
        role: &str,
        trusted: &BTreeSet<String>,
    ) -> Option<Id> {
        for endpoint in self.aliases.endpoints(id) {
            if self.store.resolve(self.snapshot, endpoint, "read").is_err() {
                continue;
            }
            for (signature, record) in self.inventory.attestations(endpoint) {
                if verify_attestation(record, endpoint, scope, role, "research-policy-1", trusted)
                    .is_ok()
                {
                    return Some(signature);
                }
            }
        }
        None
    }
}
