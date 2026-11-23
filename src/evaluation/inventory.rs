use super::{Query, Receipt};
use crate::{
    migration::Equivalence,
    profile::{CALIBRATION_EVENT_IDENTITY, Document, Kind, verify_attestation},
    store::{Archive, Snapshot},
    wire::Id,
};
use anyhow::{Result, ensure};
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
    complete_bytes: Option<usize>,
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
        for &id in snapshot.objects.iter().filter(|id| id.format().is_some()) {
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
            Err(_) => store.may_be_notice(snapshot, id),
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
    ) -> Result<()> {
        if snapshot.objects.len() <= query.budget
            && self
                .complete_bytes
                .is_some_and(|size| size <= query.byte_limit)
        {
            out.visible_notices.extend(&self.notices);
            return Ok(());
        }

        let mut hidden = false;
        let mut bytes_left = query.byte_limit;
        for (index, &id) in snapshot.objects.iter().enumerate() {
            if index >= query.budget {
                out.frontier.insert(id);
                continue;
            }
            if id.format().is_none() {
                continue;
            }
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
        self.complete_bytes = Some(query.byte_limit - bytes_left);
        Ok(())
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
