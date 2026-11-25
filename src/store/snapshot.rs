//! Snapshot identity, local catalog storage, and scoped manifest reads.
use super::Archive;
use crate::{
    profile::PUBLIC,
    wire::{Id, Record},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ops::Bound::{Excluded, Unbounded},
};

use super::{
    Manifest,
    custody::{BATCH_BYTES, Item, MEMBERS, Member, tree::Tree},
    inventory::{self, Pages},
};
use crate::profile::{Document, allows};
use serde::{Deserializer, Serializer};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ArchivedInventory {
    version: u8,
    source: String,
    parents: BTreeSet<Id>,
    objects: BTreeSet<Id>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotView {
    pub source: String,
    pub parents: BTreeSet<Id>,
    pub objects: BTreeSet<Id>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    view: SnapshotView,
    pub(crate) manifest: Option<Manifest>,
    pub(crate) pages: Pages,
    archived_version: u8,
    inherited_access: Vec<Id>,
}
impl std::ops::Deref for Snapshot {
    type Target = SnapshotView;
    fn deref(&self) -> &SnapshotView {
        &self.view
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Portable<M, P> {
    manifest: M,
    pages: P,
}
impl Serialize for Snapshot {
    fn serialize<S: Serializer>(&self, out: S) -> std::result::Result<S::Ok, S::Error> {
        let m = self.manifest.as_ref().ok_or_else(|| {
            serde::ser::Error::custom("historical evidence requires migration for exchange")
        })?;
        Portable {
            manifest: m,
            pages: &self.pages,
        }
        .serialize(out)
    }
}
impl<'de> Deserialize<'de> for Snapshot {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let Portable { manifest, pages } = Portable::<Manifest, Pages>::deserialize(d)?;
        let (members, _) = manifest.witness(&pages).map_err(serde::de::Error::custom)?;
        Ok(Self::current(manifest, members, pages))
    }
}
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum StoredHeader {
    Current(Manifest),
    Archived(ArchivedInventory),
}
impl StoredHeader {
    pub(super) fn into_snapshot(self) -> Snapshot {
        match self {
            Self::Current(m) => Snapshot::current(m, BTreeSet::new(), Pages::new()),
            Self::Archived(a) => Snapshot {
                view: SnapshotView {
                    source: a.source,
                    parents: a.parents,
                    objects: a.objects,
                },
                manifest: None,
                pages: Pages::new(),
                archived_version: a.version,
                inherited_access: Vec::new(),
            },
        }
    }
}
impl Snapshot {
    fn current(manifest: Manifest, objects: BTreeSet<Id>, pages: Pages) -> Self {
        Self {
            view: SnapshotView {
                source: manifest.source.clone(),
                parents: manifest.parents.clone(),
                objects,
            },
            manifest: Some(manifest),
            pages,
            archived_version: 0,
            inherited_access: Vec::new(),
        }
    }
    pub(super) fn hydrate(&mut self, pages: &Pages) -> Result<()> {
        if let Some(m) = &self.manifest {
            (self.view.objects, self.pages) = m.witness(pages)?;
        }
        Ok(())
    }
    pub(super) fn load_pages(&mut self, db: &Connection) -> Result<()> {
        if let Some(m) = &self.manifest {
            (self.view.objects, self.pages) = m.read(db)?;
        }
        Ok(())
    }
    pub(crate) fn replay_header(&self) -> Result<(Vec<u8>, &[Id])> {
        Ok((self.bytes()?, &self.inherited_access))
    }
    pub(crate) fn restore_replay(
        header: &[u8],
        inherited_access: Vec<Id>,
        pages: &Pages,
    ) -> Result<Self> {
        let mut cut = serde_json::from_slice::<StoredHeader>(header)?.into_snapshot();
        cut.hydrate(pages)?;
        cut.inherited_access = inherited_access;
        Ok(cut)
    }
    pub(crate) fn ensure_hydrated(&self) -> Result<()> {
        ensure!(
            self.manifest
                .as_ref()
                .is_none_or(|m| !self.pages.is_empty() && m.count == self.objects.len() as u64),
            "snapshot inventory witnesses unavailable"
        );
        Ok(())
    }
    /// Compact header committed by the head identifier. A transport snapshot also carries witnesses.
    pub fn manifest(&self) -> Result<&Manifest> {
        self.manifest
            .as_ref()
            .context("historical evidence has a flat manifest")
    }
    pub fn id(&self) -> Result<Id> {
        Ok(Id::blob(&self.bytes()?))
    }
    pub(super) fn bytes(&self) -> Result<Vec<u8>> {
        if let Some(m) = &self.manifest {
            return m.bytes();
        }
        Ok(serde_json::to_vec(&ArchivedInventory {
            version: self.archived_version,
            source: self.source.clone(),
            parents: self.parents.clone(),
            objects: self.objects.clone(),
        })?)
    }
    fn validate_header(&self) -> Result<()> {
        ensure!(!self.source.is_empty(), "invalid snapshot");
        if let Some(m) = &self.manifest {
            ensure!(
                self.source == m.source && self.parents == m.parents,
                "changed snapshot header"
            );
        } else {
            ensure!(self.archived_version == 1, "invalid snapshot");
        }
        Ok(())
    }
    pub(super) fn verify_head(&self, id: Id) -> Result<()> {
        self.validate_header()?;
        ensure!(self.id()? == id, "invalid snapshot manifest");
        if let Some(m) = &self.manifest {
            let (ids, _) = m.witness(&self.pages)?;
            ensure!(ids == self.objects, "inconsistent snapshot inventory");
        }
        Ok(())
    }
    pub(crate) fn records(&self) -> impl Iterator<Item = Id> + '_ {
        self.records_after(None)
    }

    pub(crate) fn records_after(&self, after: Option<Id>) -> impl Iterator<Item = Id> + '_ {
        // The blob range sits between the two record formats in identifier order.
        let first_blob = Id::from_blob_digest([0; 32]);
        let last_blob = Id::from_blob_digest([u8::MAX; 32]);
        let before_blobs = after.is_none_or(|id| id < first_blob).then(|| {
            self.objects
                .range((after.map_or(Unbounded, Excluded), Excluded(first_blob)))
        });
        let after_blobs = self.objects.range((
            Excluded(after.map_or(last_blob, |id| id.max(last_blob))),
            Unbounded,
        ));
        before_blobs
            .into_iter()
            .flatten()
            .chain(after_blobs)
            .copied()
    }
}

enum InventoryView<'a> {
    Materialized(&'a Snapshot),
    Paged(Manifest),
}

/// Read an authorized snapshot through either its supplied witnesses or its local pages.
pub(crate) struct InventoryReader<'a> {
    archive: &'a Archive,
    view: InventoryView<'a>,
    pub(super) tree: Tree<'a>,
}

impl<'a> InventoryReader<'a> {
    pub(crate) fn materialized(archive: &'a Archive, snapshot: &'a Snapshot) -> Self {
        Self {
            archive,
            view: InventoryView::Materialized(snapshot),
            tree: Tree::new(&archive.db),
        }
    }

    pub(crate) fn paged(archive: &'a Archive, manifest: Manifest) -> Self {
        Self {
            archive,
            view: InventoryView::Paged(manifest),
            tree: Tree::new(&archive.db),
        }
    }

    pub(crate) fn id(&self) -> Result<Id> {
        match &self.view {
            InventoryView::Materialized(snapshot) => snapshot.id(),
            InventoryView::Paged(manifest) => manifest.id(),
        }
    }

    pub(crate) fn source(&self) -> &str {
        match &self.view {
            InventoryView::Materialized(snapshot) => &snapshot.source,
            InventoryView::Paged(manifest) => &manifest.source,
        }
    }

    pub(crate) fn contains(&mut self, id: Id) -> Result<bool> {
        match &self.view {
            InventoryView::Materialized(snapshot) => Ok(snapshot.objects.contains(&id)),
            InventoryView::Paged(manifest) => {
                Ok(self.tree.get(manifest.inventory, &id.to_string())? == Some(id))
            }
        }
    }

    pub(super) fn has_exchange_scope(&self) -> Result<bool> {
        if let InventoryView::Materialized(snapshot) = &self.view
            && !snapshot.inherited_access.is_empty()
        {
            return Ok(true);
        }
        Ok(self.archive.exchange_access_scope(self.id()?)?.is_some())
    }

    fn covers(&mut self, required: &Snapshot) -> Result<bool> {
        let manifest = match &self.view {
            InventoryView::Materialized(view) => {
                return Ok(required.objects.is_subset(&view.objects));
            }
            InventoryView::Paged(manifest) => manifest,
        };
        let count = required.objects.len() as u64;
        if count == 0 {
            return Ok(true);
        }
        if count > manifest.count {
            return Ok(false);
        }
        if required
            .manifest
            .as_ref()
            .is_some_and(|m| m.inventory == manifest.inventory)
        {
            return Ok(count == manifest.count);
        }

        // Sparse horizons are cheaper to verify by authenticated point lookup.
        // For similarly sized cuts, merge the ordered stream without retaining the selected cut.
        if count <= manifest.count / 8 {
            for &id in &required.objects {
                if self.tree.get(manifest.inventory, &id.to_string())? != Some(id) {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        let mut wanted = required.objects.iter().copied();
        let mut next = wanted.next();
        manifest.scan_with(
            None,
            |id| self.tree.read(id),
            |id| {
                if let Some(target) = next {
                    if id > target {
                        return Ok(false);
                    }
                    if id == target {
                        next = wanted.next();
                    }
                }
                Ok(next.is_some())
            },
        )?;
        Ok(next.is_none())
    }

    pub(crate) fn resolve(&mut self, id: Id) -> Result<(Vec<u8>, Id)> {
        self.resolve_for(id, "read")
    }

    fn resolve_for(&mut self, id: Id, purpose: &str) -> Result<(Vec<u8>, Id)> {
        match &self.view {
            InventoryView::Materialized(snapshot) => {
                self.archive
                    .resolve_with(snapshot, id, purpose == "read", |policy| {
                        allows(policy, purpose)
                    })
            }
            InventoryView::Paged(manifest) => self.archive.resolve_member_in(
                manifest,
                id,
                purpose,
                purpose == "read",
                &mut self.tree,
            ),
        }
    }

    pub(crate) fn read(&mut self, id: Id) -> Result<Document> {
        self.read_for(id, "read")
    }

    pub(crate) fn read_for(&mut self, id: Id, purpose: &str) -> Result<Document> {
        self.archive
            .read_with(id, |key| Ok(self.resolve_for(key, purpose)?.0))
    }

    pub(crate) fn explain(
        &mut self,
        roots: &[Id],
        purpose: &str,
        budget: usize,
        depth: usize,
    ) -> super::Closure {
        self.archive
            .dependency_closure(roots, budget, depth, None, |key| {
                Ok(self.resolve_for(key, purpose)?.0)
            })
    }

    pub(crate) fn readable(&mut self, id: Id) -> Result<Id> {
        match &self.view {
            InventoryView::Materialized(snapshot) => self.archive.readable(snapshot, id),
            InventoryView::Paged(manifest) => self.archive.readable_with(
                id,
                |key| Ok(self.tree.get(manifest.inventory, &key.to_string())? == Some(key)),
                |access| {
                    self.archive
                        .require_point_scope(id, access, Some(manifest.id()?))?;
                    self.archive
                        .require_exchange_access(manifest.id()?, id, access, true)
                },
            ),
        }
    }

    pub(crate) fn records_after(&mut self, after: Option<Id>, limit: usize) -> Result<Vec<Id>> {
        match &self.view {
            InventoryView::Materialized(snapshot) => {
                Ok(snapshot.records_after(after).take(limit).collect())
            }
            InventoryView::Paged(manifest) => manifest.record_page(&mut self.tree, after, limit),
        }
    }
}

/// Earlier evaluators accepted the contents of certain hash-verified linked JSON manifests.
#[derive(Clone, Copy)]
pub(crate) enum ManifestEncoding {
    Canonical,
    LegacyJson,
}

impl ManifestEncoding {
    fn decode(self, id: Id, bytes: &[u8]) -> Result<Snapshot> {
        let snapshot = serde_json::from_slice::<StoredHeader>(bytes)?.into_snapshot();
        if matches!(self, Self::Canonical) {
            snapshot.validate_header()?;
            ensure!(snapshot.id()? == id, "invalid snapshot manifest");
        }
        Ok(snapshot)
    }
}

// A caller owns the write transaction. A previously known head retains its local status.
/// Call after verifying and installing every supplied witness in the admission transaction.
pub(super) fn save_imported_head(db: &Connection, head: Id, snapshot: &Snapshot) -> Result<()> {
    let id = head.to_string();
    if db.execute(
        "INSERT OR IGNORE INTO heads VALUES(?,?)",
        params![id, snapshot.bytes()?],
    )? != 0
    {
        db.execute("INSERT INTO rep_imported_heads VALUES(?)", [&id])?;
    }
    Ok(())
}

impl Archive {
    pub(super) fn upgrade_full_seals(&self) -> Result<()> {
        let (withdrawal_cursor, indexed_parents): (bool, bool) = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('archive_seals') WHERE name='withdrawal_serial'),
                    EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='archive_seal_parents')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if !withdrawal_cursor {
            self.db
                .execute_batch("ALTER TABLE archive_seals ADD COLUMN withdrawal_serial INTEGER;")?;
        }
        if !indexed_parents {
            self.db.execute_batch(
                "CREATE TABLE archive_seal_parents(
                    source TEXT NOT NULL,parent TEXT NOT NULL,PRIMARY KEY(source,parent)
                 ) WITHOUT ROWID;
                 INSERT OR IGNORE INTO archive_seal_parents
                    SELECT s.source,p.value FROM archive_seals s,json_each(s.parents) p;
                 CREATE TRIGGER archive_seal_index_parents AFTER INSERT ON archive_seals
                 BEGIN
                    INSERT OR IGNORE INTO archive_seal_parents
                        SELECT NEW.source,value FROM json_each(NEW.parents);
                 END;",
            )?;
        }
        Ok(())
    }

    pub(crate) fn inventory_reader(&self, head: Id) -> Result<InventoryReader<'_>> {
        Ok(InventoryReader::paged(self, self.active_manifest(head)?))
    }

    pub(crate) fn linked_snapshot(
        &self,
        view: &Snapshot,
        id: Id,
        encoding: ManifestEncoding,
    ) -> Result<Snapshot> {
        self.linked_snapshot_in(&mut InventoryReader::materialized(self, view), id, encoding)
    }

    pub(crate) fn linked_snapshot_in(
        &self,
        view: &mut InventoryReader<'_>,
        id: Id,
        encoding: ManifestEncoding,
    ) -> Result<Snapshot> {
        let mut snapshot = self.load_snapshot_packet(id, &view.resolve(id)?.0, encoding)?;
        if let InventoryView::Materialized(parent) = &view.view {
            snapshot
                .inherited_access
                .clone_from(&parent.inherited_access);
        }
        if let Some(scope) = self.exchange_access_scope(view.id()?)?
            && !snapshot.inherited_access.contains(&scope)
        {
            snapshot.inherited_access.push(scope);
        }
        Ok(snapshot)
    }

    pub(super) fn require_snapshot_exchange_access(
        &self,
        snapshot: &Snapshot,
        id: Id,
        access: Id,
        point_read: bool,
    ) -> Result<()> {
        if let Some(manifest) = &snapshot.manifest {
            self.require_exchange_access(manifest.id()?, id, access, point_read)?;
        }
        for &scope in &snapshot.inherited_access {
            self.require_exchange_access(scope, id, access, point_read)?;
        }
        Ok(())
    }

    fn load_snapshot_packet(
        &self,
        id: Id,
        bytes: &[u8],
        encoding: ManifestEncoding,
    ) -> Result<Snapshot> {
        let mut s = encoding.decode(id, bytes)?;
        s.load_pages(&self.db)?;
        Ok(s)
    }

    pub fn deposit_manifest(&self, snapshot: &Snapshot) -> Result<Id> {
        if let Some(m) = &snapshot.manifest {
            return self.blob(&m.bytes()?, Id::blob(PUBLIC));
        }
        // Frozen training requests may need to deposit an already archived manifest as an
        // evidence blob. Copy the verified original; historical inventories have no writer.
        let id = snapshot.id()?;
        let original: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT manifest FROM archive_historical WHERE id=?",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let bytes = match original {
            Some(b) => b,
            None => {
                self.raw(id)?
                    .context("historical manifest bytes unavailable")?
                    .0
            }
        };
        id.verify(&bytes)?;
        self.blob(&bytes, Id::blob(PUBLIC))
    }

    pub fn snapshot(&self, source: &str, parents: BTreeSet<Id>) -> Result<Snapshot> {
        self.head(self.snapshot_id(source, parents)?)
    }

    /// Seal all locally available members without materializing the inventory. A compatible
    /// full parent reuses its trees and reads only catalog rows deposited since that parent.
    pub fn snapshot_id(&self, source: &str, parents: BTreeSet<Id>) -> Result<Id> {
        let tx = if self.db.is_autocommit() {
            Some(rusqlite::Transaction::new_unchecked(
                &self.db,
                rusqlite::TransactionBehavior::Immediate,
            )?)
        } else {
            None
        };
        let (water, epoch) = self.archive_revision()?;
        let encoded = serde_json::to_string(&parents)?;
        let existing: Option<String> = self
            .db
            .query_row(
                "SELECT id FROM archive_seals
             WHERE source=? AND parents=? AND high_water=? AND epoch=? ORDER BY rowid DESC LIMIT 1",
                params![source, encoded, water, epoch],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            let id = id.parse()?;
            if !self.full_seal_includes_points(id, water)? {
                return Ok(id);
            }
        }
        let mut base = None;
        let mut after = 0_i64;
        for parent in &parents {
            if let Some((previous, parent_epoch, withdrawal_serial)) = self
                .db
                .query_row(
                    "SELECT high_water,epoch,coalesce(withdrawal_serial,0)
                     FROM archive_seals WHERE id=? AND source=?",
                    params![parent.to_string(), source],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()?
                && (base.is_none() || previous > after)
                && !self.full_seal_includes_points(*parent, previous)?
                && (parent_epoch == epoch
                    || self.full_parent_available(*parent, previous, withdrawal_serial)?)
            {
                base = Some(*parent);
                after = previous;
            }
        }
        let mut q = self.db.prepare(
            "SELECT d.id FROM archive_deposits d JOIN objects o ON o.id=d.id
             WHERE d.serial>? AND d.serial<=? AND o.data IS NOT NULL
               AND NOT EXISTS(SELECT 1 FROM archive_point_only WHERE id=d.id) ORDER BY d.serial",
        )?;
        let rows = q.query_map(params![after, water], |r| r.get::<_, String>(0))?;
        let ids = rows.map(|r| r?.parse());
        let head = self.seal_ids(source, &parents, base, ids, &BTreeSet::new())?;
        self.db.execute(
            "INSERT INTO archive_seals(id,source,parents,high_water,epoch,withdrawal_serial)
             VALUES(?,?,?,?,?,(SELECT coalesce(max(serial),0) FROM custody_withdrawal_log))
             ON CONFLICT(id) DO UPDATE SET high_water=excluded.high_water,epoch=excluded.epoch,
                 withdrawal_serial=excluded.withdrawal_serial",
            params![head.to_string(), source, encoded, water, epoch],
        )?;
        if let Some(tx) = tx {
            tx.commit()?
        }
        Ok(head)
    }

    fn full_seal_includes_points(&self, head: Id, high_water: i64) -> Result<bool> {
        // New disclosures never enter archive_deposits. Only legacy admissions
        // can leave a point-only member in a full cut that predates its upgrade.
        let mut query = self
            .db
            .prepare_cached("SELECT id FROM archive_point_only WHERE legacy_serial<=?")?;
        let mut rows = query.query([high_water])?;
        let Some(first) = rows.next()? else {
            return Ok(false);
        };
        let inventory = self.custody_head(head)?.inventory;
        let mut tree = Tree::new(&self.db);
        let mut row = Some(first);
        while let Some(item) = row {
            if tree.get(inventory, &item.get::<_, String>(0)?)?.is_some() {
                return Ok(true);
            }
            row = rows.next()?;
        }
        Ok(false)
    }

    fn full_parent_available(
        &self,
        parent: Id,
        high_water: i64,
        withdrawal_serial: i64,
    ) -> Result<bool> {
        // The parent already excludes withdrawals recorded at its seal. Historical seals
        // without a journal position inspect the complete withdrawal log.
        let mut query = self.db.prepare_cached(
            "SELECT w.object FROM custody_withdrawal_log w WHERE w.serial>?1 AND EXISTS (
                 SELECT 1 FROM archive_deposits d WHERE d.id=w.object AND d.serial<=?2
             )",
        )?;
        let mut rows = query.query(params![withdrawal_serial, high_water])?;
        let Some(first) = rows.next()? else {
            return Ok(true);
        };
        let inventory = self.custody_head(parent)?.inventory;
        let mut tree = Tree::new(&self.db);
        let mut row = Some(first);
        while let Some(withdrawn) = row {
            let id: String = withdrawn.get(0)?;
            if tree.get(inventory, &id)?.is_some() {
                return Ok(false);
            }
            row = rows.next()?;
        }
        Ok(true)
    }
    /// Continue one explicitly requested full lineage. Resolve the tip and seal under one lock.
    pub fn continue_snapshot(&self, source: &str) -> Result<Id> {
        self.continue_local_tip(Some(source))
    }

    /// Continue a full local tip, inferring the source when only one is eligible.
    pub fn continue_local_tip(&self, source: Option<&str>) -> Result<Id> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        // A withdrawal may rebuild the custody tree. Full seal parents still identify
        // the tips it continued even when the physical ancestry had to be rebuilt.
        let mut query = self.db.prepare(
            "WITH live_tips AS (
                 SELECT t.head,s.high_water,s.epoch,t.source FROM custody_tips t
                 LEFT JOIN archive_seals s ON s.id=t.head
                 WHERE (?1 IS NULL OR t.source=?1) AND NOT EXISTS (
                     SELECT 1 FROM archive_seal_parents p
                     WHERE p.source=t.source AND p.parent=t.head
                 )
             )
             SELECT head,high_water,epoch,source FROM live_tips
             WHERE ?1 IS NOT NULL OR source IN (
                 SELECT source FROM live_tips WHERE high_water IS NOT NULL
             )",
        )?;
        let mut rows = query.query([source])?;
        let mut full = None;
        let ambiguity = if source.is_some() {
            "continuation requires exactly one full local tip; select --parent explicitly"
        } else {
            "continuation requires exactly one full local tip; specify the source or --parent"
        };
        while let Some(row) = rows.next()? {
            let id: Id = row.get::<_, String>(0)?.parse()?;
            if let Some(water) = row.get::<_, Option<i64>>(1)? {
                ensure!(full.is_none(), "{ambiguity}");
                full = Some((id, water, row.get::<_, i64>(2)?, row.get::<_, String>(3)?));
            } else {
                let tip = self.custody_head(id)?;
                // Selected cuts build a new tree. Explicit custody appends bootstrap a
                // lineage or name their physical parent as their sole origin.
                let append = tip.bootstrap
                    || tip.parent.is_some_and(|parent| {
                        tip.origins.len() == 1 && tip.origins.contains(&parent)
                    });
                ensure!(!append, "{ambiguity}");
            }
        }
        drop(rows);
        let (parent, water, epoch, source) = full.context(ambiguity)?;
        let head = if (water, epoch) == self.archive_revision()?
            && !self.full_seal_includes_points(parent, water)?
        {
            parent
        } else {
            self.snapshot_id(&source, [parent].into())?
        };
        tx.commit()?;
        Ok(head)
    }

    pub(super) fn archive_revision(&self) -> Result<(i64, i64)> {
        Ok(self.db.query_row(
            "SELECT (SELECT coalesce(max(serial),0) FROM archive_deposits),withdrawn
             FROM archive_epoch WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    pub(super) fn latest_full_seal(&self) -> Result<Option<(Id, String, i64, i64)>> {
        let seal: Option<(String, String, i64, i64)> = self
            .db
            .query_row(
                "SELECT id,source,high_water,epoch FROM archive_seals ORDER BY rowid DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        seal.map(|(id, source, water, epoch)| Ok((id.parse()?, source, water, epoch)))
            .transpose()
    }

    /// Describe the current local work without sealing or preparing search data.
    pub fn status(&self) -> Result<super::ArchiveStatus> {
        self.with_read_view(|| {
            let full = self.latest_full_seal()?;
            let pin = self.search_pin()?;
            let search_head = self.default_search_head(pin, full.as_ref().map(|seal| seal.0))?;
            let points = super::point::catalog(&self.db)?;
            let pending_objects: i64 = self
                .db
                .prepare_cached(&format!(
                    "SELECT count(*) FROM archive_deposits d JOIN objects o ON o.id=d.id
                 WHERE d.serial>? AND o.data IS NOT NULL
                   AND NOT EXISTS(SELECT 1 FROM ({points}) p WHERE p.id=d.id)"
                ))?
                .query_row([full.as_ref().map_or(0, |seal| seal.2)], |row| row.get(0))?;
            let (_, current_epoch) = self.archive_revision()?;
            Ok(super::ArchiveStatus {
                search_head,
                search_pinned: pin.is_some(),
                pending_objects: u64::try_from(pending_objects)?,
                full_snapshot: full.map(|(id, source, _, epoch)| super::WorkingSnapshot {
                    id,
                    source,
                    withdrawals_since: current_epoch - epoch,
                }),
            })
        })
    }

    pub(super) fn register_snapshot(&self, m: &Manifest, imported: bool) -> Result<()> {
        let id = m.id()?.to_string();
        if self.db.execute(
            "INSERT OR IGNORE INTO heads VALUES(?,?)",
            params![id, m.bytes()?],
        )? != 0
            && imported
        {
            self.db
                .execute("INSERT INTO rep_imported_heads VALUES(?)", [id])?;
        }
        Ok(())
    }
    pub fn seal_members(
        &self,
        source: &str,
        parents: BTreeSet<Id>,
        objects: BTreeSet<Id>,
    ) -> Result<Snapshot> {
        self.seal_members_withheld(source, parents, objects, &BTreeSet::new())
    }

    pub(super) fn seal_members_withheld(
        &self,
        source: &str,
        parents: BTreeSet<Id>,
        objects: BTreeSet<Id>,
        withheld: &BTreeSet<Id>,
    ) -> Result<Snapshot> {
        ensure!(
            withheld.is_subset(&objects),
            "withheld member outside inventory"
        );
        let tx = if self.db.is_autocommit() {
            Some(rusqlite::Transaction::new_unchecked(
                &self.db,
                rusqlite::TransactionBehavior::Immediate,
            )?)
        } else {
            None
        };
        let head = self.seal_ids(
            source,
            &parents,
            None,
            objects.iter().copied().map(Ok),
            withheld,
        )?;
        let snapshot = self.head(head)?;
        ensure!(snapshot.objects == objects, "sealed inventory differs");
        if !withheld.is_empty() {
            let key = head.to_string();
            let mut record = self
                .db
                .prepare_cached("INSERT OR IGNORE INTO archive_review_withheld VALUES(?,?)")?;
            for id in withheld {
                record.execute(params![key, id.to_string()])?;
            }
        }
        if let Some(tx) = tx {
            tx.commit()?
        }
        Ok(snapshot)
    }
    fn seal_ids(
        &self,
        source: &str,
        parents: &BTreeSet<Id>,
        mut cursor: Option<Id>,
        ids: impl Iterator<Item = Result<Id>>,
        withheld: &BTreeSet<Id>,
    ) -> Result<Id> {
        let mut items = Vec::new();
        let mut total = 0;
        for id in ids {
            let id = id?;
            let raw = if withheld.contains(&id) {
                None
            } else {
                self.raw(id)?
            };
            let absent = raw.is_none();
            let (bytes, access) = match raw {
                Some(pair) => pair,
                None => {
                    let known: Option<String> = self
                        .db
                        .query_row(
                            "SELECT access FROM objects WHERE id=?",
                            [id.to_string()],
                            |r| r.get(0),
                        )
                        .optional()?;
                    (
                        Vec::new(),
                        known.map_or(Ok(Id::blob(PUBLIC)), |s| s.parse())?,
                    )
                }
            };
            if !items.is_empty() && (items.len() == MEMBERS || total + bytes.len() > BATCH_BYTES) {
                cursor = Some(
                    self.admit_batch(source, cursor, &items, parents, false)?
                        .head,
                );
                items.clear();
                total = 0;
            }
            total += bytes.len();
            items.push(Item {
                member: Member {
                    id,
                    access,
                    position: None,
                    absent,
                },
                bytes,
            });
        }
        let head = self
            .admit_batch(source, cursor, &items, parents, false)?
            .head;
        let manifest = self.custody_head(head)?.manifest()?;
        self.register_snapshot(&manifest, false)?;
        Ok(head)
    }
    /// Seal the readable forward dependency closure of explicitly selected objects.
    pub fn selected_snapshot(
        &self,
        source: &str,
        parents: BTreeSet<Id>,
        roots: BTreeSet<Id>,
    ) -> Result<Snapshot> {
        ensure!(!roots.is_empty(), "no snapshot roots selected");
        let tx = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let mut pending: Vec<_> = roots.iter().copied().collect();
        let mut objects = roots;
        while let Some(id) = pending.pop() {
            let (bytes, access) = self
                .raw(id)?
                .context("selected object or dependency is unavailable")?;
            self.check_access(access, |policy| crate::profile::allows(policy, "read"))?;
            let record = id
                .format()
                .map(|format| Record::decode(&bytes, format))
                .transpose()?;
            // Blob access is custody metadata outside the hashed payload. Record
            // inputs also carry the profile and each ordered evidence reference.
            for dependency in std::iter::once(access).chain(record.iter().flat_map(Record::inputs))
            {
                if objects.insert(dependency) {
                    pending.push(dependency);
                }
            }
        }
        let snapshot = self.seal_members(source, parents, objects)?;
        tx.commit()?;
        Ok(snapshot)
    }

    pub fn save(&self, s: &Snapshot) -> Result<Id> {
        if s.manifest.is_none() {
            return self.convert_archived(s.id()?, s);
        }
        s.validate_header()?;
        // A standalone snapshot supplies no historical manifest packets. Store its main proof.
        let (members, pages) = s.manifest()?.witness(&s.pages)?;
        ensure!(members == s.objects, "inconsistent snapshot inventory");
        inventory::install(&self.db, &pages)?;
        let bytes = s.bytes()?;
        let id = Id::blob(&bytes);
        self.db.execute(
            "INSERT OR IGNORE INTO heads VALUES(?,?)",
            params![id.to_string(), bytes],
        )?;
        Ok(id)
    }
    /// Follow the migration mapping for active operations. Historical evaluators use head()
    /// to resolve an exact content-addressed snapshot.
    pub fn active_head(&self, id: Id) -> Result<Snapshot> {
        let s = self.head(self.current_head_id(id)?)?;
        if s.manifest.is_some() {
            Ok(s)
        } else {
            self.head(self.convert_archived(id, &s)?)
        }
    }
    pub(super) fn active_manifest(&self, head: Id) -> Result<Manifest> {
        let head = self.current_head_id(head)?;
        let header: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT manifest FROM heads WHERE id=?",
                [head.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(header) = header {
            let m: Manifest = serde_json::from_slice(&header)?;
            m.verify(head)?;
            return Ok(m);
        }
        if let Ok(h) = self.custody_head(head) {
            return h.manifest();
        }
        Ok(self.active_head(head)?.manifest()?.clone())
    }
    pub(super) fn resolve_member(
        &self,
        m: &Manifest,
        id: Id,
        purpose: &str,
        point_read: bool,
    ) -> Result<(Vec<u8>, Id)> {
        self.resolve_member_in(
            m,
            id,
            purpose,
            point_read,
            &mut super::custody::tree::Tree::new(&self.db),
        )
    }
    pub(super) fn resolve_member_in(
        &self,
        m: &Manifest,
        id: Id,
        purpose: &str,
        point_read: bool,
        tree: &mut super::custody::tree::Tree<'_>,
    ) -> Result<(Vec<u8>, Id)> {
        ensure!(
            tree.get(m.inventory, &id.to_string())? == Some(id),
            "outside snapshot"
        );
        self.resolve_inventory_member(m, id, purpose, point_read, tree)
    }
    /// The caller has just visited this member in a verified membership walk.
    pub(super) fn resolve_inventory_member(
        &self,
        m: &Manifest,
        id: Id,
        purpose: &str,
        point_read: bool,
        tree: &mut super::custody::tree::Tree<'_>,
    ) -> Result<(Vec<u8>, Id)> {
        let (bytes, access) = self.raw_catalog_bytes(id)?.context("unavailable")?;
        ensure!(
            tree.get(m.inventory, &access.to_string())? == Some(access),
            "missing access descriptor"
        );
        let head = m.id()?;
        self.require_point_scope(id, access, point_read.then_some(head))?;
        self.require_exchange_access(head, id, access, point_read)?;
        self.check_access(access, |policy| crate::profile::allows(policy, purpose))?;
        Ok((bytes, access))
    }
    /// Check a payload and its access descriptor through their authenticated inventory paths.
    pub fn active_manifest_id(&self, head: Id) -> Result<Id> {
        self.active_manifest(head)?.id()
    }

    /// Resolve and decode both the record and its profile against the same inventory and custody cut.
    pub fn read_head(&self, head: Id, id: Id, purpose: &str) -> Result<crate::profile::Document> {
        self.with_read_view(|| self.read_with(id, |input| self.point_bytes(head, input, purpose)))
    }

    pub fn resolve_head(&self, head: Id, id: Id, purpose: &str) -> Result<Vec<u8>> {
        self.with_read_view(|| self.point_bytes(head, id, purpose))
    }
    pub fn head(&self, id: Id) -> Result<Snapshot> {
        let mut snapshot = self.snapshot_header(id)?;
        snapshot.load_pages(&self.db)?;
        Ok(snapshot)
    }

    pub(crate) fn snapshot_header(&self, id: Id) -> Result<Snapshot> {
        let b: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT manifest FROM heads WHERE id=?1 UNION ALL SELECT manifest FROM archive_historical WHERE id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        if b.is_none()
            && let Ok(h) = self.custody_head(id)
        {
            return Ok(StoredHeader::Current(h.manifest()?).into_snapshot());
        }
        let b = match b {
            Some(b) => b,
            None => {
                self.raw(id)?
                    .filter(|(_, acl)| *acl == Id::blob(PUBLIC))
                    .context("unknown snapshot")?
                    .0
            }
        };
        id.verify(&b)?;
        ManifestEncoding::Canonical.decode(id, &b)
    }

    /// Resolve a linked receipt inventory using only evidence in the caller's view.
    pub(crate) fn receipt_horizon(&self, view: &Snapshot, id: Id) -> Result<Snapshot> {
        self.receipt_horizon_in(&mut InventoryReader::materialized(self, view), id)
    }

    pub(crate) fn receipt_horizon_in(
        &self,
        view: &mut InventoryReader<'_>,
        id: Id,
    ) -> Result<Snapshot> {
        let snapshot = self.linked_snapshot_in(view, id, ManifestEncoding::Canonical)?;
        ensure!(
            view.covers(&snapshot)?,
            "receipt inventory outside the selected horizon"
        );
        Ok(snapshot)
    }

    pub(crate) fn local_receipt_horizon(&self, id: Id) -> Result<Snapshot> {
        // A linked packet can carry witnesses while relying on its enclosing cut for access.
        // Direct replay uses only a horizon the archive already recognizes on its own.
        let admitted: bool = self
            .db
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM heads WHERE id=?1
                UNION ALL SELECT 1 FROM archive_historical WHERE id=?1
                UNION ALL SELECT 1 FROM custody_heads WHERE id=?1)",
            )?
            .query_row([id.to_string()], |row| row.get(0))?;
        ensure!(
            admitted,
            "receipt horizon was not independently admitted; specify its enclosing snapshot"
        );
        self.load_snapshot_packet(id, &self.resolve_local(id)?, ManifestEncoding::Canonical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgraded_full_seals_preserve_history_and_exclude_old_point_admissions() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("legacy point catalog.sqlite");
        let source = Archive::memory()?;
        let public = Id::blob(PUBLIC);
        let bytes = b"previously cataloged point observation";
        let id = source.blob(bytes, public)?;
        let point_head = source.snapshot_id("origin", Default::default())?;
        let proof = source.export_member(point_head, id)?;
        let archive = Archive::open(&path)?;
        // Reproduce the old catalog entry and its point grant before upgrade.
        archive.blob(bytes, public)?;
        archive.import_member(point_head, &proof)?;
        let cached = archive.snapshot_id("old cached selection", Default::default())?;
        let previous = archive.snapshot_id("old continuation", Default::default())?;
        assert!(archive.head(cached)?.objects.contains(&id));
        archive.db.execute_batch(
            "INSERT INTO archive_member_access SELECT * FROM archive_point_access;
             DROP TABLE archive_point_access;
             DROP TRIGGER archive_catalog_append;
             DROP TABLE archive_point_only;
             CREATE TRIGGER archive_catalog_append AFTER INSERT ON objects
             BEGIN INSERT OR IGNORE INTO archive_deposits(id) VALUES(NEW.id); END;",
        )?;
        drop(archive);

        let reader = Archive::open_for_read(&path)?;
        assert_eq!(reader.status()?.pending_objects, 0);
        assert_eq!(reader.resolve_head(point_head, id, "read")?, bytes);
        drop(reader);

        let archive = Archive::open(&path)?;
        assert!(archive.full_seal_includes_points(cached, i64::MAX)?);
        archive.db.execute_batch(
            "DROP INDEX archive_point_legacy_deposit;
             ALTER TABLE archive_point_only DROP COLUMN legacy_serial;",
        )?;
        drop(archive);

        let archive = Archive::open(&path)?;
        let rebuilt = archive.snapshot_id("old cached selection", Default::default())?;
        assert_ne!(rebuilt, cached);
        assert!(archive.head(cached)?.objects.contains(&id));
        assert!(!archive.head(rebuilt)?.objects.contains(&id));
        assert_eq!(
            archive.snapshot_id("old cached selection", Default::default())?,
            rebuilt
        );

        let continued = archive.continue_snapshot("old continuation")?;
        assert_ne!(continued, previous);
        assert!(archive.head(previous)?.objects.contains(&id));
        let current = archive.head(continued)?;
        assert_eq!(current.parents, [previous].into());
        assert!(!current.objects.contains(&id));
        assert_eq!(archive.continue_snapshot("old continuation")?, continued);
        Ok(())
    }

    #[test]
    fn continuation_preserves_logical_parents_and_withdrawals_after_upgrade() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("archive.sqlite");
        let archive = Archive::open(&path)?;
        let public = Id::blob(PUBLIC);
        let withdrawn = archive.blob(b"superseded observation", public)?;
        let retained = archive.blob(b"retained observation", public)?;
        let first = archive.snapshot_id("research", Default::default())?;
        archive.withdraw(withdrawn)?;

        let rebuilt = archive.continue_snapshot("research")?;
        assert!(archive.custody_head(rebuilt)?.parent.is_none());
        assert_eq!(archive.head(rebuilt)?.parents, [first].into());
        assert_eq!(archive.custody_tips("research", None, 10)?.len(), 2);
        archive.db.execute_batch(
            "DROP TRIGGER archive_seal_index_parents;
             DROP TABLE archive_seal_parents;
             ALTER TABLE archive_seals DROP COLUMN withdrawal_serial;",
        )?;
        drop(archive);

        let archive = Archive::open(&path)?;
        assert_eq!(archive.continue_snapshot("research")?, rebuilt);
        let outside = archive.blob(b"temporary observation outside the parent", public)?;
        archive.withdraw(outside)?;
        let continued = archive.continue_snapshot("research")?;
        assert_eq!(archive.custody_head(continued)?.parent, Some(rebuilt));
        assert_eq!(
            archive.resolve_head(continued, retained, "read")?,
            b"retained observation"
        );
        drop(archive);

        let archive = Archive::open(&path)?;
        archive.withdraw(retained)?;
        let next = archive.continue_snapshot("research")?;
        assert!(archive.custody_head(next)?.parent.is_none());
        assert_eq!(archive.head(next)?.parents, [continued].into());
        assert!(!archive.head(next)?.objects.contains(&retained));
        assert_eq!(archive.continue_snapshot("research")?, next);
        Ok(())
    }
}
