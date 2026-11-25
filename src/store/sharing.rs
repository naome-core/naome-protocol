//! Complete inventories explicitly released to a configured replication group.
use super::{
    Archive, Snapshot, insert,
    inventory::{InvalidInventory, Page, Pages, bound_parents},
    snapshot::save_imported_head,
};
use crate::{
    profile::{Document, Kind, PROFILE, allows, verify_attestation},
    wire::{Id, Record},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::{BTreeMap, BTreeSet};

pub use super::inventory::MAX_MANIFEST_PARENTS;
pub const CATALOG_PAGE_SIZE: usize = 32;
pub const MAX_MEMBERS: usize = 512;
pub const MAX_MEMBER_BYTES: usize = 1024 * 1024;
pub const MAX_RELEASE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
struct ReleaseUnavailable(&'static str);

impl std::fmt::Display for ReleaseUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ReleaseUnavailable {}

fn charge_page(total: usize, page: &Page) -> Result<usize> {
    let total = total + serde_json::to_vec(page)?.len();
    if total > MAX_RELEASE_BYTES {
        return Err(ReleaseUnavailable("release witnesses exceed network limit").into());
    }
    Ok(total)
}

fn charge_member(total: usize, size: usize) -> Result<usize> {
    if size > MAX_MEMBER_BYTES {
        return Err(ReleaseUnavailable("object exceeds network limit").into());
    }
    let total = total + size;
    if total > MAX_RELEASE_BYTES {
        return Err(ReleaseUnavailable("release exceeds network limit").into());
    }
    Ok(total)
}

fn check_manifest_bounds(snapshot: &Snapshot) -> Result<()> {
    let count = snapshot
        .manifest
        .as_ref()
        .map_or(snapshot.objects.len() as u64, |manifest| manifest.count);
    ensure!(
        snapshot.source.len() <= 256
            && snapshot.parents.len() <= MAX_MANIFEST_PARENTS
            && (1..=MAX_MEMBERS as u64).contains(&count),
        "invalid or oversized release manifest"
    );
    Ok(())
}

fn check_manifest(head: Id, snapshot: &Snapshot) -> Result<()> {
    check_manifest_bounds(snapshot)?;
    snapshot
        .verify_head(head)
        .context("invalid or oversized release manifest")
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleasePage {
    pub revision: u64,
    pub total: usize,
    pub heads: Vec<Id>,
    pub more: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogState {
    pub revision: u64,
    pub available: usize,
}

/// Exact object bytes. Records carry their own access reference in the envelope.
#[derive(Debug)]
pub struct Member {
    pub id: Id,
    pub blob_access: Option<Id>,
    pub bytes: Vec<u8>,
}

/// Holds checked bytes until the complete inventory can be admitted atomically.
pub struct Replica {
    head: Id,
    snapshot: Snapshot,
    missing: BTreeSet<Id>,
    members: BTreeMap<Id, (Id, Vec<u8>)>,
    bytes: usize,
}

impl Replica {
    pub fn new(head: Id, snapshot: Snapshot) -> Result<Self> {
        check_manifest(head, &snapshot)?;
        let proof_bytes = snapshot.pages.values().try_fold(0, charge_page)?;
        Ok(Self {
            head,
            missing: snapshot.objects.clone(),
            snapshot,
            members: BTreeMap::new(),
            bytes: proof_bytes,
        })
    }

    pub fn next_member(&self) -> Option<Id> {
        self.missing.first().copied()
    }

    pub fn accept(&mut self, expected: Id, member: Member) -> Result<()> {
        ensure!(
            self.missing.contains(&expected) && member.id == expected,
            "unsolicited object"
        );
        let bytes = member.bytes;
        let total = charge_member(self.bytes, bytes.len())?;
        expected.verify(&bytes)?;
        let access = match (expected.format(), member.blob_access) {
            (Some(format), None) => Record::decode(&bytes, format)?.access,
            (None, Some(access)) => access,
            _ => anyhow::bail!("object class and access metadata disagree"),
        };
        ensure!(
            access.format().is_none() && self.snapshot.objects.contains(&access),
            "access descriptor outside release"
        );
        self.bytes = total;
        self.members.insert(expected, (access, bytes));
        self.missing.remove(&expected);
        Ok(())
    }

    pub fn commit(self, archive: &Archive) -> Result<Id> {
        ensure!(self.missing.is_empty(), "incomplete release");
        let tx = Transaction::new_unchecked(&archive.db, TransactionBehavior::Immediate)?;
        let witnesses = super::inventory::witness::Witnesses::begin(&tx)?;
        for (&id, page) in &self.snapshot.pages {
            witnesses.install(id, page)?;
        }
        witnesses.walk(self.snapshot.manifest()?)?;
        for (id, (access, bytes)) in self.members {
            insert(&tx, id, &bytes, access)?;
            witnesses.link(id, &bytes)?;
        }
        witnesses.finish()?;
        save_imported_head(&tx, self.head, &self.snapshot)?;
        archive.register_release(self.head, &self.snapshot)?;
        tx.commit()?;
        Ok(self.head)
    }
}

impl Archive {
    pub fn releases(&self) -> Result<BTreeSet<Id>> {
        self.id_set("SELECT head FROM available_releases")
    }

    pub fn catalog_state(&self) -> Result<CatalogState> {
        // Both tables are append-only. Each admission or first suspension changes the revision.
        let (observed, suspended): (i64, i64) = self.db.query_row(
            "SELECT (SELECT count(*) FROM releases), (SELECT count(*) FROM suspended_releases)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(CatalogState {
            revision: observed as u64 + suspended as u64,
            available: usize::try_from(observed - suspended)?,
        })
    }

    pub fn release_page(&self, after: Option<Id>) -> Result<ReleasePage> {
        let tx = self.db.unchecked_transaction()?;
        let state = self.catalog_state()?;
        let mut heads = tx
            .prepare(
                "SELECT head FROM releases r WHERE head > ? AND NOT EXISTS
             (SELECT 1 FROM suspended_releases s WHERE s.head=r.head)
             ORDER BY head LIMIT ?",
            )?
            .query_map(
                rusqlite::params![
                    after.map(|id| id.to_string()).unwrap_or_default(),
                    CATALOG_PAGE_SIZE as i64 + 1
                ],
                |row| row.get::<_, String>(0),
            )?
            .map(|row| row?.parse())
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        let more = heads.len() > CATALOG_PAGE_SIZE;
        heads.truncate(CATALOG_PAGE_SIZE);
        Ok(ReleasePage {
            revision: state.revision,
            total: state.available,
            heads,
            more,
        })
    }

    /// A transient cursor for gossip. Reconciliation always uses content identifiers.
    pub fn latest_announcement_cursor(&self) -> Result<i64> {
        Ok(self
            .db
            .query_row("SELECT coalesce(max(rowid), 0) FROM releases", [], |row| {
                row.get(0)
            })?)
    }

    pub fn announcements_after(&self, cursor: i64) -> Result<Vec<(i64, Id)>> {
        self.db
            .prepare(
                "SELECT rowid,head FROM releases r WHERE rowid > ? AND NOT EXISTS
             (SELECT 1 FROM suspended_releases s WHERE s.head=r.head)
             ORDER BY rowid LIMIT ?",
            )?
            .query_map(rusqlite::params![cursor, CATALOG_PAGE_SIZE as i64], |row| {
                Ok((row.get(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (cursor, head) = row?;
                Ok((cursor, head.parse()?))
            })
            .collect()
    }

    pub fn has_observed_release(&self, head: Id) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM releases WHERE head=?)",
            [head.to_string()],
            |row| row.get(0),
        )?)
    }

    /// Includes historical memberships of releases that no longer qualify for delivery.
    pub fn observed_releases(&self) -> Result<BTreeSet<Id>> {
        self.id_set("SELECT head FROM releases")
    }

    fn ensure_released(&self, head: Id) -> Result<()> {
        let released: bool = self.db.query_row(
            "SELECT EXISTS(SELECT head FROM releases WHERE head=?1
                           EXCEPT SELECT head FROM suspended_releases WHERE head=?1)",
            [head.to_string()],
            |r| r.get(0),
        )?;
        ensure!(released, "inventory has not been released");
        Ok(())
    }
    /// A bounded network release includes membership witnesses needed for linked scientific cuts.
    pub fn released_manifest(&self, head: Id) -> Result<Snapshot> {
        self.with_read_view(|| {
            self.ensure_released(head)?;
            let mut snapshot = self.head(head)?;
            snapshot.pages = self.release_pages(head, &snapshot)?;
            Ok(snapshot)
        })
    }

    fn release_pages(&self, head: Id, snapshot: &Snapshot) -> Result<Pages> {
        check_manifest(head, snapshot)?;
        let mut total = 0;
        let mut proof = Pages::new();
        let mut tree = super::custody::tree::Tree::new(&self.db);
        let mut include = |m: &super::Manifest, total: &mut usize| {
            m.walk(
                |id| {
                    let page = tree.read(id)?;
                    if let std::collections::btree_map::Entry::Vacant(entry) = proof.entry(id) {
                        *total = charge_page(*total, &page)?;
                        entry.insert(page.clone());
                    }
                    Ok(page)
                },
                |_| Ok(()),
            )
        };
        include(snapshot.manifest()?, &mut total)?;
        for &id in &snapshot.objects {
            if self.point_only(id)? {
                return Err(ReleaseUnavailable("member requires independent admission").into());
            }
            let size = self.size(id)?;
            total = charge_member(total, size)?;
            if id.format().is_none() && size <= 32768 {
                let (bytes, _) = self.resolve_with(snapshot, id, false, |p| allows(p, "read"))?;
                if let Some(m) = super::Manifest::from_packet(id, &bytes) {
                    include(&m, &mut total).map_err(|err| {
                        if err.is::<InvalidInventory>() {
                            err.context(ReleaseUnavailable("invalid linked inventory witness"))
                        } else if matches!(
                            err.downcast_ref::<rusqlite::Error>(),
                            Some(rusqlite::Error::QueryReturnedNoRows)
                        ) {
                            err.context(ReleaseUnavailable("missing inventory page"))
                        } else {
                            err
                        }
                    })?;
                }
            }
        }
        Ok(proof)
    }

    pub fn released_member(&self, head: Id, id: Id) -> Result<Member> {
        let tx = self.db.unchecked_transaction()?;
        self.ensure_released(head)?;
        let m = self.active_manifest(head)?;
        ensure!(
            self.size(id)? <= MAX_MEMBER_BYTES,
            "object exceeds network limit"
        );
        let (bytes, access) = self.resolve_member(&m, id, "read", false)?;
        if let Some(format) = id.format() {
            ensure!(
                Record::decode(&bytes, format)?.access == access,
                "stored access disagrees with record"
            );
        }
        tx.commit()?;
        Ok(Member {
            id,
            blob_access: id.format().is_none().then_some(access),
            bytes,
        })
    }

    pub fn release(&self, head: Id) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let head = self.current_head_id(head)?;
        let mut snapshot = self.snapshot_header(head)?;
        check_manifest_bounds(&snapshot)?;
        snapshot.load_pages(&self.db)?;
        self.save(&snapshot)?;
        self.register_release(head, &snapshot)?;
        tx.commit()?;
        Ok(())
    }

    fn index_release_members(&self, head: Id, s: &Snapshot) -> Result<()> {
        let mut insert = self
            .db
            .prepare_cached("INSERT OR IGNORE INTO archive_release_members VALUES(?,?)")?;
        let head = head.to_string();
        for id in &s.objects {
            insert.execute(params![id.to_string(), head])?;
        }
        Ok(())
    }

    // Both local publication and replica admission must be able to serve the complete cut.
    fn register_release(&self, head: Id, s: &Snapshot) -> Result<()> {
        self.release_pages(head, s)?;
        for &id in &s.objects {
            let (bytes, _) = self.resolve_with(s, id, false, |p| allows(p, "read"))?;
            if let Some(format) = id.format() {
                let record = Record::decode(&bytes, format)?;
                if record.profile == Id::blob(PROFILE) && record.kind == Kind::Attestation as u64 {
                    let d = Document::decode(&record)?;
                    // Integrity of the signing operation. A scientific evaluator
                    // independently decides whether this role/key is admissible.
                    verify_attestation(
                        &d,
                        d.one("signed-original")?,
                        &d.scope,
                        &d.field::<String>("role")?,
                        &d.field::<String>("context")?,
                        &[d.subject.clone()].into(),
                    )?;
                }
            }
        }
        self.db.execute(
            "INSERT OR IGNORE INTO releases(head) VALUES(?)",
            [head.to_string()],
        )?;
        self.index_release_members(head, s)?;
        Ok(())
    }

    /// Carry historical observations forward under the current delivery limits.
    pub(super) fn migrate_release(&self, old: Id, head: Id, s: &Snapshot) -> Result<()> {
        let suspended: Option<bool> = self
            .db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM suspended_releases WHERE head=?1)
                 FROM releases WHERE head=?1",
                [old.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(suspended) = suspended else {
            return Ok(());
        };
        let suspend = suspended
            || match self.release_pages(head, s) {
                Ok(_) => false,
                Err(err) if err.is::<ReleaseUnavailable>() => true,
                Err(err) => return Err(err),
            };
        self.db.execute(
            "INSERT OR IGNORE INTO releases(head) VALUES(?)",
            [head.to_string()],
        )?;
        if suspend {
            self.db.execute(
                "INSERT OR IGNORE INTO suspended_releases VALUES(?)",
                [head.to_string()],
            )?;
        }
        self.index_release_members(head, s)
    }
}

#[derive(Debug, serde::Serialize)]
pub struct ReplicaReview {
    pub review_horizon: Id,
    pub review_receipt: Id,
    pub extraction: crate::learning::Extraction,
}

impl Archive {
    fn seal_review_union(&self, parents: BTreeSet<Id>) -> Result<Id> {
        let mut objects = BTreeSet::new();
        for &head in &parents {
            objects.extend(&self.head(head)?.objects);
        }
        self.seal_members("local replica review", parents, objects)?
            .id()
    }

    /// Re-evaluate a local standing training request across its original
    /// inventory and the complete releases visible to this custodian.
    pub fn review_releases(
        &self,
        request: &crate::learning::TrainingRequest,
    ) -> Result<ReplicaReview> {
        use crate::{learning::LabelStanding, wire::Format};
        ensure!(
            request.standing == LabelStanding::Current,
            "replica review requires current standing"
        );
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let mut parents = self.observed_releases()?;
        parents.insert(request.label_head);
        let parents = bound_parents(parents, |group| self.seal_review_union(group))?;
        let horizon = self.seal_review_union(parents)?;
        let mut review = request.clone();
        review.label_head = horizon;
        let extraction = self.training_manifest(&review, Format::A)?;
        let mut links = vec![
            crate::profile::link("extraction", extraction.manifest),
            crate::profile::link("label-horizon", horizon),
        ];
        for (&label, reason) in &extraction.withheld {
            let mut edge = crate::profile::link("withheld-label", label);
            edge.qualifier = reason.clone();
            links.push(edge);
        }
        let audit = Document::new(
            Kind::Manifest,
            "local training review",
            "replica-review-1",
            serde_json::json!({"request":request}),
            links,
        )?;
        let review_receipt = self.document(&audit, Format::A, Id::blob(crate::profile::PUBLIC))?;
        tx.commit()?;
        Ok(ReplicaReview {
            review_horizon: horizon,
            review_receipt,
            extraction,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::PUBLIC;

    #[test]
    fn oversized_publication_is_rejected_before_inventory_loading() -> Result<()> {
        let archive = Archive::memory()?;
        for index in 0..=MAX_MEMBERS {
            archive.blob(&index.to_be_bytes(), Id::blob(PUBLIC))?;
        }
        let head = archive.snapshot_id("oversized release", BTreeSet::new())?;
        let root = archive.custody_head(head)?.inventory;
        archive
            .db
            .execute("DELETE FROM custody_pages WHERE id=?", [root.to_string()])?;
        let error = archive.release(head).unwrap_err();
        assert_eq!(error.to_string(), "invalid or oversized release manifest");
        assert!(archive.observed_releases()?.is_empty());
        Ok(())
    }
}
