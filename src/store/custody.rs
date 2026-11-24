//! Authenticated archive admission, immutable laboratory lineages and indexed reads.
mod frame;
mod replication;
mod review;
pub(super) mod tree;
pub use replication::Pull;
pub use review::{Federation, Review};
pub use tree::PageIo;

use super::{Archive, ObjectAvailability, insert, validate};
use crate::{
    profile::{Document, MEASUREMENT_PROFILE, PROFILE, PUBLIC, READ_ONLY, allows},
    wire::Id,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use tree::Tree;

pub const BLOCK: usize = 256 * 1024;
pub const MEMBERS: usize = 64;
pub const BATCH_BYTES: usize = 4 * 1024 * 1024;
const SCHEMA: &str = include_str!("custody/schema.sql");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub stream: Id,
    pub block: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub id: Id,
    pub access: Id,
    pub position: Option<Position>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub absent: bool,
}
pub struct Item {
    pub member: Member,
    pub bytes: Vec<u8>,
}
impl Item {
    pub fn blob(bytes: Vec<u8>, access: Id) -> Self {
        Self {
            member: Member {
                id: Id::blob(&bytes),
                access,
                position: None,
                absent: false,
            },
            bytes,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Head {
    pub origins: BTreeSet<Id>,
    pub inventory: Id,
    pub count: u64,
    pub bootstrap: bool,
    pub source: String,
    pub parent: Option<Id>,
    pub ancestors: Vec<Id>,
    pub sequence: u64,
    pub tree: Id,
    pub delta: Vec<Member>,
    pub missing: u64,
    pub unresolved: BTreeSet<Id>,
    pub opaque: u64,
    pub restricted: bool,
}
impl Head {
    pub fn manifest(&self) -> Result<super::Manifest> {
        let parents = self.origins.clone();
        Ok(super::Manifest {
            source: self.source.clone(),
            parents,
            inventory: self.inventory,
            count: self.count,
            custody: Id::blob(&serde_json::to_vec(self)?),
        })
    }
}

/// A verified inventory and its complete reverse index for declared record links.
pub(crate) struct BacklinkIndex<'a> {
    archive: &'a Archive,
    manifest: super::Manifest,
    root: Id,
    tree: Tree<'a>,
}

impl BacklinkIndex<'_> {
    pub(crate) fn candidates(&mut self, target: Id, limit: usize) -> Result<Vec<Id>> {
        let mut found = Vec::new();
        self.tree
            .prefix(self.root, &format!("r/{target}/"), limit, &mut found)?;
        Ok(found)
    }

    pub(crate) fn read(&mut self, id: Id) -> Result<Document> {
        self.archive.read_with(id, |id| {
            Ok(self
                .archive
                .resolve_member_in(&self.manifest, id, "read", &mut self.tree)?
                .0)
        })
    }

    pub(crate) fn readable(&mut self, id: Id) -> Result<()> {
        self.archive.readable_with(id, |key| {
            Ok(self.tree.get(self.manifest.inventory, &key.to_string())? == Some(key))
        })
    }
}
#[derive(Serialize)]
pub struct Admission {
    pub head: Id,
    pub sequence: u64,
    pub io: PageIo,
}

fn object(id: Id) -> String {
    format!("o/{id}")
}
fn required(id: Id) -> String {
    format!("d/{id}")
}
fn position(slot: &Position) -> String {
    format!("b/{}/{:016x}", slot.stream, slot.block)
}

impl Archive {
    /// Open an existing source for queries or frame export without performing schema or bootstrap writes.
    pub fn custody_reader(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let db = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        db.busy_timeout(std::time::Duration::from_secs(10))?;
        Ok(Self { db })
    }
    pub(super) fn init_custody(&self) -> Result<()> {
        self.db.execute_batch(SCHEMA)?;
        Ok(())
    }

    pub fn custody_head(&self, id: Id) -> Result<Head> {
        self.custody_head_if_present(id)?
            .context("unadmitted paged head")
    }

    pub(crate) fn backlink_index(&self, head: Id) -> Result<Option<BacklinkIndex<'_>>> {
        let Some(proof) = self
            .custody_head_if_present(head)?
            .filter(|proof| proof.opaque == 0)
        else {
            return Ok(None);
        };
        Ok(Some(BacklinkIndex {
            archive: self,
            manifest: proof.manifest()?,
            root: proof.tree,
            tree: Tree::new(&self.db),
        }))
    }
    pub(super) fn custody_head_if_present(&self, id: Id) -> Result<Option<Head>> {
        self.custody_head_data(id)?
            .map(|data| {
                let h: Head = serde_json::from_slice(&data)?;
                h.manifest()?.verify(id)?;
                Ok(h)
            })
            .transpose()
    }
    fn custody_head_data(&self, id: Id) -> Result<Option<Vec<u8>>> {
        self.db
            .prepare_cached("SELECT data FROM custody_heads WHERE id=?")?
            .query_row([id.to_string()], |r| r.get(0))
            .optional()
            .map_err(Into::into)
    }
    fn custody_head_bytes(&self, id: Id) -> Result<Vec<u8>> {
        self.custody_head_data(id)?.context("unadmitted paged head")
    }

    /// Independently authored children of the same parent each retain an addressable head.
    pub fn custody_append(
        &self,
        source: &str,
        parent: Option<Id>,
        items: &[Item],
    ) -> Result<Admission> {
        let tx = rusqlite::Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let admission = self.admit_batch(
            source,
            parent,
            items,
            &parent.into_iter().collect(),
            parent.is_none(),
        )?;
        self.register_snapshot(&self.custody_head(admission.head)?.manifest()?, false)?;
        tx.commit()?;
        Ok(admission)
    }

    pub(super) fn admit_batch(
        &self,
        source: &str,
        parent: Option<Id>,
        items: &[Item],
        origins: &BTreeSet<Id>,
        bootstrap_members: bool,
    ) -> Result<Admission> {
        self.admit_custody_head(source, parent, items, origins, bootstrap_members, true)
    }

    /// A historical ancestry group is addressable by its head but has no live branch.
    pub(super) fn seal_ancestry_group(&self, source: &str, origins: &BTreeSet<Id>) -> Result<Id> {
        Ok(self
            .admit_custody_head(source, None, &[], origins, false, false)?
            .head)
    }

    fn admit_custody_head(
        &self,
        source: &str,
        parent: Option<Id>,
        items: &[Item],
        origins: &BTreeSet<Id>,
        bootstrap_members: bool,
        track_tip: bool,
    ) -> Result<Admission> {
        ensure!(
            !source.is_empty()
                && source.len() <= 256
                && items.len() <= MEMBERS
                && (!items.is_empty() || !bootstrap_members)
                && origins.len() <= super::inventory::MAX_MANIFEST_PARENTS,
            "invalid source or batch size"
        );
        let previous = parent.map(|id| self.custody_head(id)).transpose()?;
        ensure!(
            previous.as_ref().is_none_or(|h| h.source == source),
            "source changed within lineage"
        );
        let sequence = previous.as_ref().map_or(Ok(1), |h| {
            h.sequence.checked_add(1).context("sequence overflow")
        })?;
        let root = previous.as_ref().map(|h| h.tree);
        let mut missing = previous.as_ref().map_or(0, |h| h.missing);
        let mut unresolved = previous
            .as_ref()
            .map(|h| h.unresolved.clone())
            .unwrap_or_default();
        let mut opaque = previous.as_ref().map_or(0, |h| h.opaque);
        let mut restricted = previous.as_ref().is_some_and(|h| h.restricted);
        let mut tree = Tree::new(&self.db);
        let mut changes = BTreeMap::new();
        let mut added = BTreeSet::new();
        let mut references = BTreeSet::new();
        let mut pack = Vec::new();
        let mut slots = Vec::new();
        // The wire budget covers submitted bytes. Genesis derives its bootstrap locally.
        let total: usize = items.iter().map(|item| item.bytes.len()).sum();
        ensure!(items.len() == 1 || total <= BATCH_BYTES, "paged byte limit");
        let mut edges = 0_usize;
        let public = Id::blob(PUBLIC);
        let read_only = Id::blob(READ_ONLY);
        let bootstrap = if bootstrap_members {
            [PUBLIC, READ_ONLY, PROFILE, MEASUREMENT_PROFILE]
                .into_iter()
                .map(|b| Item::blob(b.to_vec(), public))
                .collect()
        } else {
            Vec::new()
        };
        let keys = bootstrap
            .iter()
            .chain(items)
            .flat_map(|item| {
                let id = item.member.id;
                [object(id), required(id)]
            })
            .collect();
        let known = tree.lookup_many(root, keys)?;
        for item in bootstrap.iter().chain(items) {
            let Member {
                id,
                access,
                position: slot,
                absent,
            } = item.member;
            if absent {
                ensure!(
                    item.bytes.is_empty() && slot.is_none(),
                    "absent member carries data"
                );
                let key = object(id);
                if changes.insert(key.clone(), access).is_none() && !known.contains_key(&key) {
                    added.insert(id);
                    opaque += 1;
                    restricted = true;
                }
                continue;
            }
            let record = validate(id, &item.bytes, access)?;
            if let Some(ref slot) = slot {
                ensure!(
                    id.format().is_none() && !item.bytes.is_empty() && item.bytes.len() <= BLOCK,
                    "invalid raw block"
                );
                references.insert(slot.stream);
                changes.insert(format!("r/{id}/{}", slot.stream), slot.stream);
                changes.insert(format!("f/{}/{id}", slot.stream), id);
                if let Some(old) = changes.insert(position(slot), id) {
                    ensure!(old == id, "conflicting raw block");
                }
            }
            let key = object(id);
            if let Some(old) = changes.insert(key.clone(), access) {
                ensure!(old == access, "conflicting access");
                continue;
            }
            if let Some(&old) = known.get(&key) {
                ensure!(old == access, "conflicting access");
                continue;
            }
            added.insert(id);
            restricted |= access != public && access != read_only;
            references.insert(access);
            if access != id {
                changes.insert(format!("r/{access}/{id}"), id);
                changes.insert(format!("f/{id}/{access}"), access);
            }
            if let Some(record) = record {
                // Large historical records remain readable. An unindexed edge leaves coverage open.
                let index_edges = edges.saturating_add(record.edges.len()) <= 4096;
                if index_edges {
                    edges += record.edges.len();
                    match Document::decode(&record) {
                        Ok(doc) if doc.kind == crate::profile::Kind::Notice => {
                            // Only exact endpoints have a complete index. Other notice forms keep
                            // negative statements open until a versioned evaluator handles them.
                            let endpoints: Vec<_> = doc
                                .inputs
                                .iter()
                                .filter(|l| {
                                    matches!(
                                        l.role.as_str(),
                                        "source-version" | "target" | "calibration"
                                    )
                                })
                                .collect();
                            if endpoints.is_empty() {
                                opaque += 1;
                            }
                            for link in &doc.inputs {
                                changes.insert(format!("c/{}/{id}", link.target), id);
                            }
                        }
                        Ok(doc) if doc.kind != crate::profile::Kind::Mapping => {}
                        _ => opaque += 1,
                    }
                    for dep in record.inputs() {
                        changes.insert(format!("r/{dep}/{id}"), id);
                        changes.insert(format!("f/{id}/{dep}"), dep);
                        references.insert(dep);
                    }
                } else {
                    opaque += 1;
                }
                let exists: bool = self
                    .db
                    .prepare_cached("SELECT EXISTS(SELECT 1 FROM custody_records WHERE id=?)")?
                    .query_row([id.to_string()], |r| r.get(0))?;
                if !exists {
                    slots.push((id, access, pack.len(), item.bytes.len()));
                    pack.extend_from_slice(&item.bytes);
                }
            } else {
                ensure!(
                    insert(&self.db, id, &item.bytes, access)? == ObjectAvailability::Available,
                    "payload withdrawn"
                );
            }
        }
        for &id in &added {
            if known.contains_key(&required(id)) {
                unresolved.remove(&id);
                missing = missing.checked_sub(1).context("invalid coverage count")?;
            }
        }
        let keys = references
            .iter()
            .filter(|id| !added.contains(id))
            .flat_map(|&id| [object(id), required(id)])
            .collect();
        let known = tree.lookup_many(root, keys)?;
        for id in references {
            let key = required(id);
            if !added.contains(&id) && !known.contains_key(&object(id)) && !known.contains_key(&key)
            {
                missing += 1;
                if unresolved.len() < 64 {
                    unresolved.insert(id);
                }
            }
            changes.insert(key, id);
        }
        if !slots.is_empty() {
            self.db
                .execute("INSERT INTO custody_packs(data) VALUES(?)", [pack])?;
            let segment = self.db.last_insert_rowid();
            for (id, access, start, len) in slots {
                let catalog=self.db.prepare_cached("INSERT INTO objects(id,data,access) VALUES(?,x'',?) ON CONFLICT(id) DO UPDATE SET data=x''
                 WHERE objects.data IS NOT NULL AND objects.access=excluded.access")?
                    .execute(params![id.to_string(),access.to_string()])?;
                ensure!(catalog == 1, "conflicting custody for packed record");
                self.db
                    .prepare_cached("INSERT INTO custody_records VALUES(?,?,?,?,?)")?
                    .execute(params![
                        id.to_string(),
                        segment,
                        start as i64,
                        len as i64,
                        access.to_string()
                    ])?;
            }
        }
        let members = added.iter().map(|id| (id.to_string(), *id)).collect();
        let inventory = tree.apply(previous.as_ref().map(|h| h.inventory), members)?;
        let count = previous.as_ref().map_or(0, |h| h.count) + added.len() as u64;
        let tree_root = tree.apply(root, changes)?;
        let head = Head {
            inventory,
            count,
            origins: origins.clone(),
            bootstrap: bootstrap_members,
            source: source.into(),
            parent,
            ancestors: self.custody_jumps(parent)?,
            sequence,
            tree: tree_root,
            delta: items.iter().map(|i| i.member.clone()).collect(),
            missing,
            unresolved,
            opaque,
            restricted,
        };
        let bytes = serde_json::to_vec(&head)?;
        let manifest = head.manifest()?;
        let id = manifest.id()?;
        if self.db.execute(
            "INSERT OR IGNORE INTO custody_heads VALUES(?,?)",
            params![id.to_string(), bytes],
        )? != 0
        {
            if track_tip {
                self.db.execute(
                    "INSERT INTO custody_tips VALUES(?,?)",
                    params![source, id.to_string()],
                )?;
                if let Some(p) = parent {
                    self.db.execute(
                        "DELETE FROM custody_tips WHERE source=? AND head=?",
                        params![source, p.to_string()],
                    )?;
                }
            }
            for (level, ancestor) in head.ancestors.iter().enumerate() {
                self.db
                    .prepare_cached("INSERT INTO custody_jumps VALUES(?,?,?)")?
                    .execute(params![id.to_string(), level as i64, ancestor.to_string()])?;
            }
        }
        Ok(Admission {
            head: id,
            sequence,
            io: tree.io,
        })
    }

    fn custody_raw(&self, id: Id) -> Result<(Vec<u8>, Id)> {
        self.raw(id)?.context("unavailable archive member")
    }
    fn read_member(
        &self,
        tree: &mut Tree<'_>,
        h: &Head,
        id: Id,
        purpose: &str,
    ) -> Result<(Vec<u8>, Id)> {
        self.read_member_under(tree, h, h.tree, id, purpose)
    }

    fn read_member_under(
        &self,
        tree: &mut Tree<'_>,
        inventory: &Head,
        policy_root: Id,
        id: Id,
        purpose: &str,
    ) -> Result<(Vec<u8>, Id)> {
        let access = tree
            .get(inventory.tree, &object(id))?
            .context("outside inventory")?;
        ensure!(
            tree.get(policy_root, &object(access))? == Some(Id::blob(PUBLIC)),
            "missing access descriptor"
        );
        self.check_access(access, |p| allows(p, purpose))?;
        let (data, actual) = self.custody_raw(id)?;
        ensure!(actual == access, "inconsistent custody descriptor");
        Ok((data, access))
    }
    /// Read permission and policy of the stream descriptor used for new measurement blocks.
    pub fn custody_stream_access(&self, head: Id, stream: Id) -> Result<Id> {
        self.with_read_view(|| {
            let h = self.custody_head(head)?;
            let mut tree = Tree::new(&self.db);
            let (_, access) = self.read_member(&mut tree, &h, stream, "read")?;
            Ok(access)
        })
    }
    pub fn custody_resolve(&self, head: Id, id: Id, purpose: &str) -> Result<Vec<u8>> {
        Ok(self.custody_resolve_counted(head, id, purpose)?.0)
    }
    pub fn custody_resolve_counted(
        &self,
        head: Id,
        id: Id,
        purpose: &str,
    ) -> Result<(Vec<u8>, PageIo)> {
        self.with_read_view(|| {
            let mut tree = Tree::new(&self.db);
            let (bytes, _) = self.read_member(&mut tree, &self.custody_head(head)?, id, purpose)?;
            Ok((bytes, tree.io))
        })
    }
    pub fn custody_range(&self, head: Id, stream: Id, offset: u64, len: usize) -> Result<Vec<u8>> {
        ensure!(len <= BATCH_BYTES, "range exceeds read budget");
        let end = offset.checked_add(len as u64).context("range overflow")?;
        self.with_read_view(|| {
            let h = self.custody_head(head)?;
            let mut tree = Tree::new(&self.db);
            self.read_member(&mut tree, &h, stream, "read")?;
            let mut out = Vec::with_capacity(len);
            let mut cursor = offset;
            while cursor < end {
                let slot = Position {
                    stream,
                    block: cursor / BLOCK as u64,
                };
                let id = tree
                    .get(h.tree, &position(&slot))?
                    .context("unavailable raw block")?;
                let (data, _) = self.read_member(&mut tree, &h, id, "read")?;
                let start = (cursor % BLOCK as u64) as usize;
                let n = (BLOCK - start).min((end - cursor) as usize);
                out.extend_from_slice(
                    data.get(start..start + n)
                        .context("range outside stored block")?,
                );
                cursor += n as u64;
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests;
