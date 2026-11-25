//! Read-purpose point disclosure. Its independently pinned cut is never registered as a full head.
use super::{Archive, Packet, validate};
use crate::{
    profile::{PUBLIC, allows},
    store::{
        ObjectAvailability,
        custody::{Head, MEMBERS, tree::Tree},
        insert_bytes,
        inventory::{Page, Pages},
        point,
    },
    wire::{Id, MAX_BYTES},
};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufReader, Read},
};

const MAX_PROOF_BYTES: usize = 4 * MAX_BYTES + 8 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberProof {
    version: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    member: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    members: Option<Vec<Id>>,
    descriptor: Head,
    pages: Pages,
    packets: Vec<Packet>,
}

/// Verify just the selected search path, including each inherited key interval.
pub(super) fn path(
    mut root: Id,
    key: &str,
    mut load: impl FnMut(Id) -> Result<Page>,
) -> Result<Id> {
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

pub(super) fn committed_access(
    head: &Head,
    member: Id,
    load: impl FnMut(Id) -> Result<Page>,
) -> Result<Id> {
    path(head.tree, &format!("o/{member}"), load)
}

impl MemberProof {
    pub fn requested(&self) -> Result<&[Id]> {
        match (self.version, &self.member, &self.members) {
            (1, Some(id), None) => Ok(std::slice::from_ref(id)),
            (2, None, Some(ids))
                if (2..=MEMBERS).contains(&ids.len()) && ids.windows(2).all(|w| w[0] < w[1]) =>
            {
                Ok(ids)
            }
            _ => anyhow::bail!("invalid member proof selection"),
        }
    }

    pub fn read(reader: impl Read) -> Result<Self> {
        let mut limited = reader.take(MAX_PROOF_BYTES as u64 + 1);
        let value = serde_json::from_reader(BufReader::new(&mut limited))?;
        ensure!(limited.limit() > 0, "member proof byte limit");
        Ok(value)
    }
}

impl Archive {
    pub fn export_member(&self, head: Id, id: Id) -> Result<MemberProof> {
        self.export_members(head, &[id])
    }

    /// The union of authenticated paths for an explicit selection from a single cut.
    pub fn export_members(&self, head: Id, ids: &[Id]) -> Result<MemberProof> {
        ensure!(
            (1..=MEMBERS).contains(&ids.len()),
            "invalid member selection size"
        );
        let mut ids = ids.to_vec();
        ids.sort_unstable();
        ensure!(
            ids.windows(2).all(|w| w[0] != w[1]),
            "duplicate requested member"
        );
        self.with_read_view(|| {
            let h = self.custody_head(head)?;
            ensure!(
                !h.restricted,
                "cannot disclose index witnesses from a restricted head"
            );
            let mut tree = Tree::new(&self.db);
            let mut pages = Pages::new();
            let mut packets = BTreeMap::new();
            let mut pending = ids.clone();
            // Conservative envelope allowance, including member IDs, names and punctuation.
            let mut wire_bytes = serde_json::to_vec(&h)?.len() + ids.len() * 80 + 256;
            while let Some(key) = pending.pop() {
                if packets.contains_key(&key) {
                    continue;
                }
                let mut load = |page| {
                    if let Some(p) = pages.get(&page) {
                        return Ok(p.clone());
                    }
                    let p = tree.read(page)?;
                    wire_bytes += serde_json::to_vec(&p)?.len() + 80;
                    ensure!(wire_bytes <= MAX_PROOF_BYTES, "member proof byte limit");
                    pages.insert(page, p.clone());
                    Ok(p)
                };
                ensure!(
                    path(h.inventory, &key.to_string(), &mut load)? == key,
                    "invalid inventory membership"
                );
                let access = committed_access(&h, key, &mut load)?;
                let bytes = self.custody_resolve(head, key, "read")?;
                if let Some(record) = validate(key, &bytes, access)? {
                    pending.push(record.profile);
                }
                pending.push(access);
                wire_bytes += bytes.len() * 2 + 192;
                ensure!(wire_bytes <= MAX_PROOF_BYTES, "member proof byte limit");
                packets.insert(
                    key,
                    Packet {
                        id: key,
                        access,
                        hex: hex::encode(bytes),
                    },
                );
            }
            let single = ids.len() == 1;
            Ok(MemberProof {
                version: if single { 1 } else { 2 },
                member: single.then(|| ids[0]),
                members: (!single).then_some(ids),
                descriptor: h,
                pages,
                packets: packets.into_values().collect(),
            })
        })
    }
    pub fn import_member(&self, head: Id, proof: &MemberProof) -> Result<Id> {
        let [id] = proof.requested()? else {
            anyhow::bail!("expected one requested member");
        };
        self.import_members(head, proof)?;
        Ok(*id)
    }

    /// Verify the exact requested union before committing all authorized bytes atomically.
    pub fn import_members(&self, head: Id, proof: &MemberProof) -> Result<Vec<Id>> {
        let requested = proof.requested()?;
        let h = &proof.descriptor;
        h.manifest()?.verify(head)?;
        ensure!(
            !h.restricted
                && (1..=5 * requested.len()).contains(&proof.packets.len())
                && proof.pages.len() <= 330 * requested.len(),
            "invalid point disclosure"
        );
        let mut used = BTreeSet::new();
        let mut expected: BTreeSet<Id> = requested.iter().copied().collect();
        let mut remaining = MAX_PROOF_BYTES;
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
                    && committed_access(h, p.id, &mut load)? == p.access,
                "unbound packet"
            );
            ensure!(p.hex.len() <= 2 * MAX_BYTES, "oversized member");
            remaining = remaining
                .checked_sub(p.hex.len())
                .context("member proof byte limit")?;
            let mut bytes = Vec::new();
            if let Some(r) = p.decode(&mut bytes)? {
                ensure!(
                    requested.binary_search(&p.id).is_ok(),
                    "unexpected additional record"
                );
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
        let head_key = head.to_string();
        let mut added_grant = false;
        for (key, (access, bytes)) in contents {
            point::mark_new(&tx, key)?;
            ensure!(
                insert_bytes(&tx, key, &bytes, access)? == ObjectAvailability::Available,
                "locally withdrawn point disclosure"
            );
            added_grant |= tx.execute(
                "INSERT OR IGNORE INTO archive_point_access VALUES(?,?,?)",
                params![head_key, key.to_string(), access.to_string()],
            )? != 0;
        }
        if added_grant {
            crate::store::search_index::invalidate_access_scope(&tx, &head_key)?;
        }
        tx.commit()?;
        Ok(requested.to_vec())
    }

    pub(in crate::store) fn point_bytes(&self, head: Id, id: Id, purpose: &str) -> Result<Vec<u8>> {
        if let Some(proved) = self.point_access(head, id)?
            && !self
                .db
                .prepare_cached("SELECT EXISTS(SELECT 1 FROM heads WHERE id=?)")?
                .query_row([head.to_string()], |row| row.get::<_, bool>(0))?
        {
            ensure!(
                purpose == "read",
                "point disclosure authorizes reading only"
            );
            let (bytes, access) = self
                .raw_catalog_bytes(id)?
                .context("unavailable point member")?;
            ensure!(access == proved, "point access changed");
            self.check_access(access, |p| allows(p, purpose))?;
            return Ok(bytes);
        }
        Ok(self
            .resolve_member(&self.active_manifest(head)?, id, purpose)?
            .0)
    }
}
