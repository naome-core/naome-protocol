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
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

pub use super::inventory::MAX_MANIFEST_PARENTS;
pub const CATALOG_PAGE_SIZE: usize = 32;
pub const MAX_MEMBERS: usize = 512;
pub const MAX_MEMBER_BYTES: usize = 1024 * 1024;
pub const MAX_RELEASE_BYTES: usize = 16 * 1024 * 1024;

pub(super) fn review_withheld_access() -> Id {
    // The reserved ID does not identify a readable access policy.
    Id::blob(b"naome-review-withheld")
}

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
    pub(super) fn upgrade_release_access(&self) -> Result<()> {
        if self.db.execute(
            "INSERT OR IGNORE INTO archive_release_access_state VALUES(1)",
            [],
        )? != 0
        {
            self.db.execute(
                "INSERT OR IGNORE INTO suspended_releases
                 SELECT r.head FROM archive_point_only p
                 CROSS JOIN archive_release_members r ON r.member=p.id",
                [],
            )?;
        }
        Ok(())
    }

    pub(super) fn suspend_release_on_exchange_access(&self, head: Id) -> Result<()> {
        let key = head.to_string();
        let available: bool = self
            .db
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM releases r WHERE r.head=?1 AND NOT EXISTS
                 (SELECT 1 FROM suspended_releases WHERE head=?1))",
            )?
            .query_row([&key], |row| row.get(0))?;
        if available {
            let mut acl = self
                .db
                .prepare_cached("SELECT access FROM objects WHERE id=?")?;
            for &id in &self.head(head)?.objects {
                let access: String = acl.query_row([id.to_string()], |row| row.get(0))?;
                if self
                    .require_exchange_access(head, id, access.parse()?, false)
                    .is_err()
                {
                    self.db
                        .execute("INSERT OR IGNORE INTO suspended_releases VALUES(?)", [&key])?;
                    break;
                }
            }
        }
        Ok(())
    }

    /// An older archive can be opened for inspection without applying the migration.
    fn catalog_suspensions(&self) -> Result<Cow<'static, str>> {
        const SUSPENDED: &str = "SELECT head FROM suspended_releases";
        let (current, indexed): (bool, bool) = self.db.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='archive_release_access_state'),
                    EXISTS(SELECT 1 FROM sqlite_schema WHERE name='archive_release_members')",
        )?.query_row([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        if current || !indexed {
            return Ok(Cow::Borrowed(SUSPENDED));
        }
        let points = super::point::catalog(&self.db)?;
        Ok(Cow::Owned(format!(
            "{SUSPENDED} UNION SELECT r.head FROM ({points}) p
             CROSS JOIN archive_release_members r ON r.member=p.id"
        )))
    }

    pub fn releases(&self) -> Result<BTreeSet<Id>> {
        let suspended = self.catalog_suspensions()?;
        self.id_set(&format!(
            "SELECT head FROM releases EXCEPT SELECT head FROM ({suspended})"
        ))
    }

    pub fn catalog_state(&self) -> Result<CatalogState> {
        // Both tables are append-only. Each admission or first suspension changes the revision.
        let access = self.catalog_suspensions()?;
        let (observed, suspended): (i64, i64) = self
            .db
            .prepare_cached(&format!(
                "SELECT (SELECT count(*) FROM releases), (SELECT count(*) FROM ({access}))",
            ))?
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(CatalogState {
            revision: observed as u64 + suspended as u64,
            available: usize::try_from(observed - suspended)?,
        })
    }

    pub fn release_page(&self, after: Option<Id>) -> Result<ReleasePage> {
        let tx = self.db.unchecked_transaction()?;
        let state = self.catalog_state()?;
        let suspended = self.catalog_suspensions()?;
        let mut heads = tx
            .prepare_cached(&format!(
                "SELECT head FROM releases r WHERE head > ? AND NOT EXISTS
                 (SELECT 1 FROM ({suspended}) s WHERE s.head=r.head)
                 ORDER BY head LIMIT ?",
            ))?
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
        let suspended = self.catalog_suspensions()?;
        self.db
            .prepare_cached(&format!(
                "SELECT rowid,head FROM releases r WHERE rowid > ? AND NOT EXISTS
                 (SELECT 1 FROM ({suspended}) s WHERE s.head=r.head)
                 ORDER BY rowid LIMIT ?",
            ))?
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
        let suspended = self.catalog_suspensions()?;
        let released: bool = self
            .db
            .prepare_cached(&format!(
                "SELECT EXISTS(SELECT 1 FROM releases WHERE head=?1 AND NOT EXISTS
                           (SELECT 1 FROM ({suspended}) s WHERE s.head=?1))",
            ))?
            .query_row([head.to_string()], |r| r.get(0))?;
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
                        } else if err.is::<super::custody::tree::MissingPage>() {
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
        let suspended: bool = self
            .db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM suspended_releases WHERE head=?)")?
            .query_row([head.to_string()], |row| row.get(0))?;
        ensure!(
            !suspended,
            "release is suspended; seal a new snapshot to publish"
        );
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
        let mut withheld = BTreeSet::new();
        let mut access = self
            .db
            .prepare_cached("SELECT access FROM objects WHERE id=? AND data IS NOT NULL")?;
        for &head in &parents {
            let snapshot = self.head(head)?;
            for &id in &snapshot.objects {
                if !objects.insert(id) && !withheld.contains(&id) {
                    continue;
                }
                let local_acl: Option<String> = access
                    .query_row([id.to_string()], |row| row.get(0))
                    .optional()?;
                let admitted = match local_acl {
                    Some(acl) => {
                        let acl = acl.parse()?;
                        snapshot.objects.contains(&acl)
                            && !self.point_only(id)?
                            && self.require_exchange_access(head, id, acl, false).is_ok()
                            && self.check_access(acl, |_| true).is_ok()
                    }
                    None => false,
                };
                if admitted {
                    withheld.remove(&id);
                } else {
                    withheld.insert(id);
                }
            }
        }
        self.seal_members_withheld("local replica review", parents, objects, &withheld)?
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
    fn upgrade_suspends_releases_containing_historical_point_bytes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("historical release.sqlite");
        let author = Archive::memory()?;
        let archive = Archive::open(&path)?;
        let public = Id::blob(PUBLIC);
        let available = archive.snapshot_id("untouched release", Default::default())?;
        archive.release(available)?;
        let bytes = b"historical point transfer";
        let member = author.blob(bytes, public)?;
        let origin = author.snapshot_id("point origin", Default::default())?;
        archive.blob(bytes, public)?;
        archive.import_member(origin, &author.export_member(origin, member)?)?;
        let unavailable = archive.snapshot_id("historical release", Default::default())?;
        archive.release(unavailable)?;
        archive.db.execute_batch(
            "INSERT INTO archive_member_access SELECT * FROM archive_point_access;
             DROP TABLE archive_point_access;
             DROP TABLE archive_release_access_state;
             DROP TRIGGER archive_catalog_append;
             DROP TABLE archive_point_only;
             CREATE TRIGGER archive_catalog_append AFTER INSERT ON objects
             BEGIN INSERT OR IGNORE INTO archive_deposits(id) VALUES(NEW.id); END;",
        )?;
        drop(archive);

        let check = |archive: &Archive| -> Result<()> {
            assert_eq!(
                archive.observed_releases()?,
                [available, unavailable].into()
            );
            assert_eq!(archive.releases()?, [available].into());
            assert_eq!(
                archive.catalog_state()?,
                CatalogState {
                    revision: 3,
                    available: 1
                }
            );
            assert_eq!(archive.release_page(None)?.heads, [available]);
            assert_eq!(
                archive
                    .announcements_after(0)?
                    .iter()
                    .map(|&(_, id)| id)
                    .collect::<Vec<_>>(),
                [available]
            );
            assert!(archive.released_manifest(unavailable).is_err());
            assert!(archive.released_member(unavailable, member).is_err());
            assert!(archive.head(unavailable)?.objects.contains(&member));
            Ok(())
        };
        check(&Archive::open_for_read(&path)?)?;
        let upgraded = Archive::open(&path)?;
        check(&upgraded)?;
        assert!(upgraded.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM suspended_releases WHERE head=?)",
            [unavailable.to_string()],
            |row| row.get::<_, bool>(0),
        )?);
        upgraded.blob(bytes, public)?;
        assert!(upgraded.release(unavailable).is_err());
        let fresh = upgraded.snapshot_id("independently readmitted release", Default::default())?;
        upgraded.release(fresh)?;
        assert_eq!(upgraded.releases()?, [available, fresh].into());
        assert!(upgraded.observed_releases()?.contains(&unavailable));
        drop(upgraded);
        assert_eq!(
            Archive::open_for_read(&path)?.releases()?,
            [available, fresh].into()
        );
        Ok(())
    }

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
