use super::{NoticeSearch, Query, Receipt, notice_coverage, rules::NoticeBudget};
use crate::{
    migration::Equivalence,
    profile::{CALIBRATION_EVENT_IDENTITY, Document, Kind, verify_attestation},
    store::{Archive, InventoryReader, Snapshot},
    wire::Id,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Readable records and their indexes for one snapshot in the current archive transaction.
#[derive(Default, Serialize, Deserialize)]
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
        budget: usize,
    ) -> (Self, BTreeSet<Id>, Option<UnscannedRecords>) {
        let mut records = snapshot.records();
        let mut inventory = Self::default();
        let mut frontier = BTreeSet::new();
        let mut after = None;
        for id in records.by_ref().take(budget) {
            after = Some(id);
            if !inventory.read(store, snapshot, id, true) {
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
        let hidden = match store.read(snapshot, id, "read") {
            Ok(document) => {
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
            Err(_) => store.may_be_notice(snapshot, id, "read"),
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
        budget_rule: &NoticeBudget,
        exclusion_keys: Option<&BTreeSet<String>>,
    ) -> Result<()> {
        let records_only = !matches!(budget_rule, NoticeBudget::Objects);
        if let Some((records, bytes)) = self.complete_scan
            && (if records_only {
                records
            } else {
                snapshot.objects.len()
            }) <= query.budget
            && bytes <= query.byte_limit
        {
            out.visible_notices.extend(&self.notices);
            if exclusion_keys.is_some() {
                out.notice_search = Some(NoticeSearch {
                    complete: true,
                    ..NoticeSearch::default()
                });
            }
            return Ok(());
        }

        let mut hidden = BTreeSet::new();
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
            if matches!(budget_rule, NoticeBudget::CompactRecords)
                && (index >= query.budget || bytes_left == 0)
            {
                out.unscanned_records_from = Some(id);
                out.frontier.insert(id);
                break;
            }
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
                hidden.insert(id);
            }
        }

        // A covered scan depends on the current recipient policy and cannot populate the
        // policy-independent cache. A cut short by budget cannot borrow cached attestations.
        let cacheable_scan = hidden.is_empty();
        if let Some(keys) = exclusion_keys {
            let mut report = NoticeSearch::default();
            if out.frontier.is_empty() {
                hidden.retain(|id| {
                    // Available bytes with an opaque or invalid profile still need inspection.
                    if store.readable(snapshot, *id).is_ok() {
                        return true;
                    }
                    if let Some((signature, _)) = self.attestations(*id).find(|(_, attestation)| {
                        notice_coverage::verify_exclusion(attestation, *id, keys).is_ok()
                    }) {
                        report.classified_occurrences.insert(*id, signature);
                        out.dependencies.insert(signature);
                        false
                    } else {
                        true
                    }
                });
            }
            report.complete = out.frontier.is_empty() && hidden.is_empty();
            report
                .unresolved_records
                .extend(out.frontier.iter().copied());
            report.unresolved_records.extend(hidden.iter().copied());
            out.notice_search = Some(report);
        }
        out.frontier.extend(hidden.iter().copied());
        ensure!(
            hidden.is_empty(),
            "notice horizon contains inaccessible or opaque records"
        );
        ensure!(out.frontier.is_empty(), "notice search budget exhausted");
        if cacheable_scan {
            self.complete_scan = Some((records, query.byte_limit - bytes_left));
        }
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
        self.with_read_view(|| {
            Self::argument_graph_report(
                InventoryReader::materialized(self, snapshot),
                target,
                purpose,
                budget,
                depth,
                GraphScan::Legacy,
            )
        })
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
        self.with_read_view(|| {
            Self::argument_graph_report(
                InventoryReader::materialized(self, snapshot),
                target,
                purpose,
                budget,
                depth,
                GraphScan::Compact { after },
            )
        })
    }

    #[cfg(feature = "cli")]
    pub(crate) fn argument_graph_at(
        &self,
        head: Id,
        target: Id,
        purpose: &str,
        budget: usize,
        depth: usize,
        after: Option<Id>,
    ) -> Result<Value> {
        self.with_read_view(|| {
            Self::argument_graph_report(
                self.inventory_reader(head)?,
                target,
                purpose,
                budget,
                depth,
                GraphScan::Compact { after },
            )
        })
    }

    fn argument_graph_report(
        mut inventory: InventoryReader<'_>,
        target: Id,
        purpose: &str,
        budget: usize,
        depth: usize,
        scan: GraphScan,
    ) -> Result<Value> {
        let mut after = match scan {
            GraphScan::Legacy => None,
            GraphScan::Compact { after } => after,
        };
        if let Some(id) = after {
            ensure!(
                id.format().is_some() && inventory.contains(id)?,
                "scan cursor is outside the snapshot record inventory"
            );
        }
        let compact = matches!(scan, GraphScan::Compact { .. });
        let mut remaining = budget;
        let mut unscanned = None;
        let mut unsearched = BTreeSet::new();
        let mut arguments = Vec::new();
        let mut notices: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
        'pages: loop {
            let limit = if compact {
                remaining.saturating_add(1).min(64)
            } else {
                64
            };
            let page = inventory.records_after(after, limit)?;
            if page.is_empty() {
                break;
            }
            for id in page {
                if remaining == 0 {
                    if compact {
                        unscanned = Some(UnscannedRecords { after, next: id });
                        break 'pages;
                    }
                    unsearched.insert(id);
                } else {
                    remaining -= 1;
                    match inventory.read_for(id, purpose) {
                        Ok(doc)
                            if doc.kind == Kind::Argument
                                && doc.roles("claim").any(|claim| claim == target) =>
                        {
                            arguments.push(id);
                        }
                        Ok(doc) if doc.kind == Kind::Notice => {
                            notices.insert(
                                id,
                                doc.inputs.into_iter().map(|edge| edge.target).collect(),
                            );
                        }
                        Ok(_) => {}
                        Err(_) => {
                            unsearched.insert(id);
                        }
                    }
                }
                after = Some(id);
            }
        }
        let roots: Vec<_> = arguments.iter().copied().chain([target]).collect();
        let graph = inventory.explain(&roots, purpose, budget, depth);
        let contrary: Vec<_> = notices
            .iter()
            .filter(|(_, targets)| !targets.is_disjoint(&graph.visited))
            .map(|(&id, _)| id)
            .collect();
        let mut report = json!({
            "snapshot": inventory.id()?,
            "source": inventory.source(),
            "target": target,
            "arguments": arguments,
            "dependencies": graph,
            "candidate_notices": contrary,
            "unsearched": unsearched,
            "budget_per_phase": budget,
        });
        if compact {
            // A notice and the argument exposing its premise may fall on different pages.
            // Keep the endpoints of every readable notice for the cross-page join.
            report["notice_targets"] = json!(notices);
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
        for endpoint in self.aliases.members(id) {
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
