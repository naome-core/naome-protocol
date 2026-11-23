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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub version: u8,
    pub source: String,
    pub parents: BTreeSet<Id>,
    pub objects: BTreeSet<Id>,
}
impl Snapshot {
    pub fn id(&self) -> Result<Id> {
        Ok(Id::blob(&self.bytes()?))
    }

    pub(super) fn bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    fn validate_header(&self) -> Result<()> {
        ensure!(
            self.version == 1 && !self.source.is_empty(),
            "invalid snapshot"
        );
        Ok(())
    }

    pub(super) fn verify_head(&self, id: Id) -> Result<()> {
        self.validate_header()?;
        ensure!(self.id()? == id, "invalid snapshot manifest");
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
        let snapshot: Snapshot = serde_json::from_slice(bytes)?;
        if matches!(self, Self::Canonical) {
            snapshot.verify_head(id)?;
        }
        Ok(snapshot)
    }
}

// A caller owns the write transaction. A previously known head retains its local status.
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
    pub(crate) fn linked_snapshot(
        &self,
        view: &Snapshot,
        id: Id,
        encoding: ManifestEncoding,
    ) -> Result<Snapshot> {
        encoding.decode(id, &self.resolve(view, id, "read")?)
    }

    pub(crate) fn deposit_manifest(&self, snapshot: &Snapshot) -> Result<Id> {
        self.blob(&snapshot.bytes()?, Id::blob(PUBLIC))
    }

    pub fn snapshot(&self, source: &str, parents: BTreeSet<Id>) -> Result<Snapshot> {
        let s = Snapshot {
            version: 1,
            source: source.into(),
            parents,
            objects: self.id_set("SELECT id FROM objects WHERE data IS NOT NULL")?,
        };
        self.save(&s)?;
        Ok(s)
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
        let snapshot = Snapshot {
            version: 1,
            source: source.into(),
            parents,
            objects,
        };
        self.save(&snapshot)?;
        tx.commit()?;
        Ok(snapshot)
    }

    pub fn save(&self, s: &Snapshot) -> Result<Id> {
        s.validate_header()?;
        let bytes = s.bytes()?;
        let id = Id::blob(&bytes);
        self.db.execute(
            "INSERT OR IGNORE INTO heads VALUES(?,?)",
            params![id.to_string(), bytes],
        )?;
        Ok(id)
    }
    pub fn head(&self, id: Id) -> Result<Snapshot> {
        let b: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT manifest FROM heads WHERE id=?",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
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
        let snapshot = self.linked_snapshot(view, id, ManifestEncoding::Canonical)?;
        ensure!(
            snapshot.objects.is_subset(&view.objects),
            "receipt inventory outside the selected horizon"
        );
        Ok(snapshot)
    }
}
