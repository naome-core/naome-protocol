//! Version-two, append-only custody views. Version-one snapshot bytes and replay remain independent.
mod frame;
mod replication;
mod review;
mod tree;
pub use replication::Pull;
pub use review::{Federation, Review};
pub use tree::PageIo;

use super::{Archive, ObjectAvailability, insert, validate};
use crate::{
    profile::{Document, MEASUREMENT_PROFILE, PROFILE, PUBLIC, READ_ONLY, allows},
    wire::{Id, Record},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use tree::Tree;

pub const BLOCK: usize = 256 * 1024;
pub const MEMBERS: usize = 64;
pub const BATCH_BYTES: usize = 4 * 1024 * 1024;
const SCHEMA: &str = include_str!("paged/schema.sql");

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
            },
            bytes,
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Head {
    pub version: u8,
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
    pub fn paged_reader(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let db = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        db.busy_timeout(std::time::Duration::from_secs(10))?;
        Ok(Self { db })
    }
    pub fn init_paged(&self) -> Result<()> {
        self.db.execute_batch(SCHEMA)?;
        Ok(())
    }

    pub fn paged_head(&self, id: Id) -> Result<Head> {
        let data: Vec<u8> = self
            .db
            .prepare_cached("SELECT data FROM rep2_heads WHERE id=?")?
            .query_row([id.to_string()], |r| r.get(0))
            .context("unadmitted paged head")?;
        id.verify(&data)?;
        Ok(serde_json::from_slice(&data)?)
    }

    /// Independently authored children of the same parent each retain an addressable head.
    pub fn paged_append(
        &self,
        source: &str,
        parent: Option<Id>,
        items: &[Item],
    ) -> Result<Admission> {
        let tx = rusqlite::Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let admission = self.admit_paged(source, parent, items)?;
        tx.commit()?;
        Ok(admission)
    }

    fn admit_paged(&self, source: &str, parent: Option<Id>, items: &[Item]) -> Result<Admission> {
        ensure!(
            !source.is_empty() && source.len() <= 256 && (1..=MEMBERS).contains(&items.len()),
            "invalid source or batch size"
        );
        let previous = parent.map(|id| self.paged_head(id)).transpose()?;
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
        let mut total = 0;
        let mut edges = 0;
        let public = Id::blob(PUBLIC);
        let read_only = Id::blob(READ_ONLY);
        let bootstrap = if parent.is_none() {
            [PUBLIC, READ_ONLY, PROFILE, MEASUREMENT_PROFILE]
                .into_iter()
                .map(|b| Item::blob(b.to_vec(), public))
                .collect()
        } else {
            Vec::new()
        };
        for item in bootstrap.iter().chain(items) {
            let Member {
                id,
                access,
                position: slot,
            } = item.member;
            total += item.bytes.len();
            ensure!(
                item.bytes.len() <= 1024 * 1024 && total <= BATCH_BYTES,
                "paged byte limit"
            );
            validate(id, &item.bytes, access)?;
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
            if let Some(old) = root.map(|r| tree.get(r, &key)).transpose()?.flatten() {
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
            if let Some(f) = id.format() {
                let record = Record::decode(&item.bytes, f)?;
                edges += record.edges.len();
                ensure!(edges <= 4096, "declared edge budget exhausted");
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
                let exists: bool = self
                    .db
                    .prepare_cached("SELECT EXISTS(SELECT 1 FROM rep2_records WHERE id=?)")?
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
            if root
                .map(|r| tree.get(r, &required(id)))
                .transpose()?
                .flatten()
                .is_some()
            {
                unresolved.remove(&id);
                missing = missing.checked_sub(1).context("invalid coverage count")?;
            }
        }
        for id in references {
            let key = required(id);
            if !added.contains(&id)
                && root
                    .map(|r| tree.get(r, &object(id)))
                    .transpose()?
                    .flatten()
                    .is_none()
                && root
                    .map(|r| tree.get(r, &key))
                    .transpose()?
                    .flatten()
                    .is_none()
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
                .execute("INSERT INTO rep2_packs(data) VALUES(?)", [pack])?;
            let segment = self.db.last_insert_rowid();
            for (id, access, start, len) in slots {
                self.db
                    .prepare_cached("INSERT INTO rep2_records VALUES(?,?,?,?,?)")?
                    .execute(params![
                        id.to_string(),
                        segment,
                        start as i64,
                        len as i64,
                        access.to_string()
                    ])?;
            }
        }
        let tree_root = tree.apply(root, changes)?;
        let head = Head {
            version: 2,
            source: source.into(),
            parent,
            ancestors: self.paged_jumps(parent)?,
            sequence,
            tree: tree_root,
            delta: items.iter().map(|i| i.member.clone()).collect(),
            missing,
            unresolved,
            opaque,
            restricted,
        };
        let bytes = serde_json::to_vec(&head)?;
        let id = Id::blob(&bytes);
        if self.db.execute(
            "INSERT OR IGNORE INTO rep2_heads VALUES(?,?)",
            params![id.to_string(), bytes],
        )? != 0
        {
            self.db.execute(
                "INSERT INTO rep2_tips VALUES(?,?)",
                params![source, id.to_string()],
            )?;
            if let Some(p) = parent {
                self.db.execute(
                    "DELETE FROM rep2_tips WHERE source=? AND head=?",
                    params![source, p.to_string()],
                )?;
            }
            for (level, ancestor) in head.ancestors.iter().enumerate() {
                self.db
                    .prepare_cached("INSERT INTO rep2_jumps VALUES(?,?,?)")?
                    .execute(params![id.to_string(), level as i64, ancestor.to_string()])?;
            }
        }
        Ok(Admission {
            head: id,
            sequence,
            io: tree.io,
        })
    }

    fn paged_raw(&self, id: Id) -> Result<(Vec<u8>, Id)> {
        if id.format().is_none() {
            return self.raw(id)?.context("unavailable payload");
        }
        let (data,acl):(Vec<u8>,String)=self.db.prepare_cached("SELECT substr(p.data,r.start+1,r.length),r.access FROM rep2_records r JOIN rep2_packs p ON p.id=r.pack WHERE r.id=?")?.query_row([id.to_string()],|r|Ok((r.get(0)?,r.get(1)?)))?;
        id.verify(&data)?;
        Ok((data, acl.parse()?))
    }
    fn read_member(&self, tree: &mut Tree<'_>, h: &Head, id: Id, purpose: &str) -> Result<Vec<u8>> {
        let access = tree
            .get(h.tree, &object(id))?
            .context("outside inventory")?;
        ensure!(
            tree.get(h.tree, &object(access))? == Some(Id::blob(PUBLIC)),
            "missing access descriptor"
        );
        self.check_access(access, |p| allows(p, purpose))?;
        let (data, actual) = self.paged_raw(id)?;
        ensure!(actual == access, "inconsistent custody descriptor");
        Ok(data)
    }
    pub fn paged_resolve(&self, head: Id, id: Id, purpose: &str) -> Result<Vec<u8>> {
        Ok(self.paged_resolve_counted(head, id, purpose)?.0)
    }
    pub fn paged_resolve_counted(
        &self,
        head: Id,
        id: Id,
        purpose: &str,
    ) -> Result<(Vec<u8>, PageIo)> {
        self.with_read_view(|| {
            let mut tree = Tree::new(&self.db);
            let bytes = self.read_member(&mut tree, &self.paged_head(head)?, id, purpose)?;
            Ok((bytes, tree.io))
        })
    }
    pub fn paged_range(&self, head: Id, stream: Id, offset: u64, len: usize) -> Result<Vec<u8>> {
        ensure!(len <= BATCH_BYTES, "range exceeds read budget");
        let end = offset.checked_add(len as u64).context("range overflow")?;
        self.with_read_view(|| {
            let h = self.paged_head(head)?;
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
                let data = self.read_member(&mut tree, &h, id, "read")?;
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
