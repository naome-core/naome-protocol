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
    custody::{BATCH_BYTES, Item, MEMBERS, Member},
    inventory::{self, Pages},
};
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
pub(super) fn save_imported_head(db: &Connection, head: Id, snapshot: &Snapshot) -> Result<()> {
    inventory::install(db, &snapshot.pages)?;
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
    pub(crate) fn linked_snapshot(
        &self,
        view: &Snapshot,
        id: Id,
        encoding: ManifestEncoding,
    ) -> Result<Snapshot> {
        let mut s = encoding.decode(id, &self.resolve(view, id, "read")?)?;
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
        let (water, epoch): (i64, i64) = self.db.query_row(
            "SELECT (SELECT coalesce(max(serial),0) FROM archive_deposits),withdrawn
             FROM archive_epoch WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let encoded = serde_json::to_string(&parents)?;
        let existing: Option<String> = self
            .db
            .query_row(
                "SELECT id FROM archive_seals
             WHERE source=? AND parents=? AND high_water=? AND epoch=?",
                params![source, encoded, water, epoch],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return id.parse();
        }
        let mut base = None;
        let mut after = 0_i64;
        for parent in &parents {
            if let Some(previous) = self
                .db
                .query_row(
                    "SELECT high_water FROM archive_seals WHERE id=? AND source=? AND epoch=?",
                    params![parent.to_string(), source, epoch],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                && (base.is_none() || previous > after)
            {
                base = Some(*parent);
                after = previous;
            }
        }
        let mut q = self.db.prepare(
            "SELECT d.id FROM archive_deposits d JOIN objects o ON o.id=d.id
             WHERE d.serial>? AND d.serial<=? AND o.data IS NOT NULL ORDER BY d.serial",
        )?;
        let rows = q.query_map(params![after, water], |r| r.get::<_, String>(0))?;
        let ids = rows.map(|r| r?.parse());
        let head = self.seal_ids(source, &parents, base, ids)?;
        self.db.execute(
            "INSERT INTO archive_seals VALUES(?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET high_water=excluded.high_water,epoch=excluded.epoch",
            params![head.to_string(), source, encoded, water, epoch],
        )?;
        if let Some(tx) = tx {
            tx.commit()?
        }
        Ok(head)
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
        let tx = if self.db.is_autocommit() {
            Some(rusqlite::Transaction::new_unchecked(
                &self.db,
                rusqlite::TransactionBehavior::Immediate,
            )?)
        } else {
            None
        };
        let head = self.seal_ids(source, &parents, None, objects.iter().copied().map(Ok))?;
        let snapshot = self.head(head)?;
        ensure!(snapshot.objects == objects, "sealed inventory differs");
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
    ) -> Result<Id> {
        let mut items = Vec::new();
        let mut total = 0;
        for id in ids {
            let id = id?;
            let raw = self.raw(id)?;
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
        s.verify_head(s.id()?)?;
        inventory::install(&self.db, &s.pages)?;
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
        Ok(self.active_head(head)?.manifest()?.clone())
    }
    pub(super) fn resolve_member(
        &self,
        m: &Manifest,
        id: Id,
        purpose: &str,
    ) -> Result<(Vec<u8>, Id)> {
        let mut tree = super::custody::tree::Tree::new(&self.db);
        ensure!(
            tree.get(m.inventory, &id.to_string())? == Some(id),
            "outside snapshot"
        );
        let (bytes, access) = self.raw(id)?.context("unavailable")?;
        ensure!(
            tree.get(m.inventory, &access.to_string())? == Some(access),
            "missing access descriptor"
        );
        self.check_access(access, |policy| crate::profile::allows(policy, purpose))?;
        Ok((bytes, access))
    }
    /// Check a payload and its access descriptor through their authenticated inventory paths.
    pub fn resolve_head(&self, head: Id, id: Id, purpose: &str) -> Result<Vec<u8>> {
        self.with_read_view(|| {
            let m = self.active_manifest(head)?;
            Ok(self.resolve_member(&m, id, purpose)?.0)
        })
    }
    pub fn head(&self, id: Id) -> Result<Snapshot> {
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
            let m = h.manifest()?;
            let (objects, pages) = m.read(&self.db)?;
            return Ok(Snapshot::current(m, objects, pages));
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
        let mut s = ManifestEncoding::Canonical.decode(id, &b)?;
        s.load_pages(&self.db)?;
        Ok(s)
    }

    /// Resolve a linked receipt inventory using only evidence in the caller's view.
    pub(crate) fn receipt_horizon(&self, view: &Snapshot, id: Id) -> Result<Snapshot> {
        let snapshot = self.linked_snapshot(view, id, ManifestEncoding::Canonical)?;
        ensure!(
            snapshot.objects.is_subset(&view.objects),
            "receipt inventory outside the selected horizon"
        );
        Ok(snapshot)
    }
}
