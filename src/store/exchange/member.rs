//! Read-purpose point disclosure. Its independently pinned cut is never registered as a full head.
use super::{Archive, Packet, validate};
use crate::{
    profile::{PUBLIC, allows},
    store::{
        ObjectAvailability,
        custody::{Head, tree::Tree},
        insert,
        inventory::{Page, Pages},
    },
    wire::{Id, MAX_BYTES},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufReader, Read},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberProof {
    version: u8,
    member: Id,
    descriptor: Head,
    pages: Pages,
    packets: Vec<Packet>,
}

/// Verify just the selected search path, including each inherited key interval.
fn path(mut root: Id, key: &str, mut load: impl FnMut(Id) -> Result<Page>) -> Result<Id> {
    let (mut low, mut high) = (None::<String>, None::<String>);
    for _ in 0..=32 {
        let page = load(root)?;
        let bytes = serde_json::to_vec(&page)?;
        ensure!(bytes.len() <= 16384, "oversized path page");
        root.verify(&bytes)?;
        let (leaf, rows) = match page {
            Page::Leaf(rows) => (true, rows),
            Page::Branch(rows) => (false, rows),
        };
        ensure!(
            (1..=32).contains(&rows.len())
                && rows.windows(2).all(|w| w[0].0 < w[1].0)
                && low.as_ref().is_none_or(|k| k == &rows[0].0)
                && high.as_ref().is_none_or(|k| &rows[rows.len() - 1].0 < k),
            "invalid path interval"
        );
        let n = rows.partition_point(|(k, _)| k.as_str() <= key);
        ensure!(n > 0, "key outside proven path");
        let (first, value) = &rows[n - 1];
        if leaf {
            ensure!(first == key, "missing proven key");
            return Ok(*value);
        }
        low = Some(first.clone());
        if let Some((next, _)) = rows.get(n) {
            high = Some(next.clone());
        }
        root = *value;
    }
    anyhow::bail!("membership proof depth limit")
}

impl MemberProof {
    pub fn read(reader: impl Read) -> Result<Self> {
        // One bounded member plus its profile, access descriptors and path witnesses.
        let mut limited = reader.take(4 * MAX_BYTES as u64 + 8 * 1024 * 1024 + 1);
        let value = serde_json::from_reader(BufReader::new(&mut limited))?;
        ensure!(limited.limit() > 0, "member proof byte limit");
        Ok(value)
    }
}

impl Archive {
    pub fn export_member(&self, head: Id, id: Id) -> Result<MemberProof> {
        self.with_read_view(|| {
            let h = self.custody_head(head)?;
            ensure!(
                !h.restricted,
                "cannot disclose index witnesses from a restricted head"
            );
            let mut tree = Tree::new(&self.db);
            let mut pages = Pages::new();
            let mut packets = BTreeMap::new();
            let mut pending = vec![id];
            while let Some(key) = pending.pop() {
                if packets.contains_key(&key) {
                    continue;
                }
                let mut load = |page| {
                    if let Some(p) = pages.get(&page) {
                        return Ok(p.clone());
                    }
                    let p = tree.read(page)?;
                    pages.insert(page, p.clone());
                    Ok(p)
                };
                ensure!(
                    path(h.inventory, &key.to_string(), &mut load)? == key,
                    "invalid inventory membership"
                );
                let access = path(h.tree, &format!("o/{key}"), &mut load)?;
                let bytes = self.custody_resolve(head, key, "read")?;
                if let Some(record) = validate(key, &bytes, access)? {
                    pending.push(record.profile);
                }
                pending.push(access);
                packets.insert(
                    key,
                    Packet {
                        id: key,
                        access,
                        hex: hex::encode(bytes),
                    },
                );
            }
            Ok(MemberProof {
                version: 1,
                member: id,
                descriptor: h,
                pages,
                packets: packets.into_values().collect(),
            })
        })
    }
    pub fn import_member(&self, head: Id, proof: &MemberProof) -> Result<Id> {
        let h = &proof.descriptor;
        h.manifest()?.verify(head)?;
        ensure!(
            proof.version == 1
                && !h.restricted
                && (1..=5).contains(&proof.packets.len())
                && proof.pages.len() <= 330,
            "invalid point disclosure"
        );
        let mut used = BTreeSet::new();
        let mut expected = BTreeSet::from([proof.member]);
        let mut contents = BTreeMap::new();
        for p in &proof.packets {
            let mut load = |id| {
                used.insert(id);
                proof
                    .pages
                    .get(&id)
                    .cloned()
                    .context("missing path witness")
            };
            ensure!(
                path(h.inventory, &p.id.to_string(), &mut load)? == p.id
                    && path(h.tree, &format!("o/{}", p.id), &mut load)? == p.access,
                "unbound packet"
            );
            ensure!(p.hex.len() <= 2 * MAX_BYTES, "oversized member");
            let bytes = hex::decode(&p.hex)?;
            if let Some(r) = validate(p.id, &bytes, p.access)? {
                ensure!(p.id == proof.member, "unexpected additional record");
                expected.insert(r.profile);
            }
            ensure!(
                contents.insert(p.id, (p.access, bytes)).is_none(),
                "duplicate point packet"
            );
        }
        // Only the requested record/blob, its profile and their necessary ACL chain may be disclosed.
        ensure!(
            contents
                .get(&Id::blob(PUBLIC))
                .is_some_and(|(outer, _)| *outer == Id::blob(PUBLIC)),
            "missing public access root"
        );
        let main: Vec<_> = expected.iter().copied().collect();
        for key in main {
            let (access, _) = contents
                .get(&key)
                .context("missing requested object or profile")?;
            let (outer, policy) = contents.get(access).context("missing access policy")?;
            ensure!(
                *outer == Id::blob(PUBLIC) && allows(policy, "read"),
                "denied point disclosure"
            );
            expected.extend([*access, *outer]);
        }
        ensure!(
            contents.keys().copied().eq(expected) && used.len() == proof.pages.len(),
            "unexpected or missing point evidence"
        );
        let tx = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        for (key, (access, bytes)) in contents {
            ensure!(
                insert(&tx, key, &bytes, access)? == ObjectAvailability::Available,
                "locally withdrawn point disclosure"
            );
            tx.execute(
                "INSERT OR IGNORE INTO archive_member_access VALUES(?,?,?)",
                params![head.to_string(), key.to_string(), access.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(proof.member)
    }

    pub(in crate::store) fn point_bytes(&self, head: Id, id: Id, purpose: &str) -> Result<Vec<u8>> {
        if !self
            .db
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='archive_member_access')",
            )?
            .query_row([], |r| r.get::<_, bool>(0))?
        {
            return Ok(self
                .resolve_member(&self.active_manifest(head)?, id, purpose)?
                .0);
        }
        let present: Option<String> = self
            .db
            .prepare_cached(
                "SELECT access FROM archive_member_access WHERE head=?1 AND member=?2
             AND NOT EXISTS(SELECT 1 FROM heads WHERE id=?1)",
            )?
            .query_row(params![head.to_string(), id.to_string()], |r| r.get(0))
            .optional()?;
        if let Some(proved) = present {
            ensure!(
                purpose == "read",
                "point disclosure authorizes reading only"
            );
            let (bytes, access) = self.raw(id)?.context("unavailable point member")?;
            ensure!(access == proved.parse()?, "point access changed");
            self.check_access(access, |p| allows(p, purpose))?;
            return Ok(bytes);
        }
        Ok(self
            .resolve_member(&self.active_manifest(head)?, id, purpose)?
            .0)
    }
}
