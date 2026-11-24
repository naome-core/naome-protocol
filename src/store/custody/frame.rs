//! A portable, length-delimited delta and an authenticated path from the externally pinned tip.
use super::*;
use std::{
    borrow::Borrow,
    io::{Read, Write},
    rc::Rc,
};

pub(super) const MAGIC: &[u8; 4] = b"NAOF";
const MAX_HEADER: usize = 32 * 1024;

fn put(out: &mut impl Write, bytes: &[u8]) -> Result<()> {
    out.write_all(&(u32::try_from(bytes.len())?).to_be_bytes())?;
    out.write_all(bytes)?;
    Ok(())
}
fn take(reader: &mut impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut len = [0; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    ensure!(len <= limit, "frame field over budget");
    let mut data = vec![0; len];
    reader.read_exact(&mut data)?;
    Ok(data)
}
fn header(reader: &mut impl Read) -> Result<(Head, Id)> {
    let bytes = take(reader, MAX_HEADER)?;
    decode_header(&bytes)
}
fn decode_header(bytes: &[u8]) -> Result<(Head, Id)> {
    let h: Head = serde_json::from_slice(bytes)?;
    ensure!(
        h.sequence != 0 && h.ancestors.len() <= 64 && h.delta.len() <= MEMBERS,
        "invalid delta descriptor"
    );
    let id = h.manifest()?.id()?;
    Ok((h, id))
}
fn level(distance: u64) -> usize {
    distance.ilog2() as usize
}

/// Each hop is matched to the authenticated child selected by the previous hop.
fn verify_path<H: Borrow<Head>>(
    target: Id,
    delta: &Head,
    next: Id,
    mut load: impl FnMut(Id) -> Result<(H, Id)>,
) -> Result<()> {
    let mut expected = target;
    let mut expected_seq = None;
    let mut hops = 0;
    while expected != next {
        hops += 1;
        ensure!(hops <= 64, "ancestry budget exhausted");
        let (proof, id) = load(expected)?;
        let proof = proof.borrow();
        ensure!(
            id == expected
                && proof.source == delta.source
                && expected_seq.is_none_or(|s| s == proof.sequence)
                && proof.sequence > delta.sequence,
            "unverified source ancestry"
        );
        let power = level(proof.sequence - delta.sequence);
        expected = *proof
            .ancestors
            .get(power)
            .context("missing authenticated skip link")?;
        expected_seq = Some(proof.sequence - (1_u64 << power));
    }
    ensure!(
        expected_seq.is_none_or(|s| s == delta.sequence),
        "invalid delta sequence"
    );
    Ok(())
}

/// Receiver-held, verified descriptors for a single direct pull. References are external pins.
#[derive(Default)]
pub(super) struct ProofCache(std::collections::VecDeque<(Id, Rc<Head>)>);

impl ProofCache {
    fn fetch(&mut self, origin: &Archive, pin: Id, bytes: &mut usize) -> Result<(Rc<Head>, Id)> {
        // A reference is one blob digest. A miss also transfers a length-prefixed descriptor.
        *bytes += 32;
        if let Some(index) = self.0.iter().position(|(id, _)| *id == pin) {
            let item = self.0.remove(index).context("missing cached descriptor")?;
            self.0.push_back(item.clone());
            return Ok((item.1, item.0));
        }
        let encoded = origin.custody_head_bytes(pin)?;
        ensure!(encoded.len() <= MAX_HEADER, "oversized proof descriptor");
        *bytes += 4 + encoded.len();
        let (desc, actual) = decode_header(&encoded)?;
        ensure!(actual == pin, "unverified proof descriptor");
        let desc = Rc::new(desc);
        if self.0.len() == 16 {
            self.0.pop_front();
        }
        self.0.push_back((pin, desc.clone()));
        Ok((desc, pin))
    }
}

impl Archive {
    /// Write the next bounded delta after a confirmed cursor. The sender checks its live grant.
    pub fn custody_export_step(
        &self,
        target: Id,
        after: Option<Id>,
        mut out: impl Write,
    ) -> Result<(usize, PageIo)> {
        self.with_read_view(|| {
            let tip = self.custody_head(target)?;
            let mut io = self.permitted_pull(target, &tip)?;
            let sequence = match after {
                Some(id) => self
                    .custody_head(id)?
                    .sequence
                    .checked_add(1)
                    .context("sequence overflow")?,
                None => 1,
            };
            ensure!(
                sequence <= tip.sequence,
                "cursor is current or outside lineage"
            );
            // The delta goes first; the proof walks authenticated skip links from target back to it.
            let distance = tip.sequence - sequence;
            let next = self.ancestor(target, distance)?;
            let delta = self.custody_head(next)?;
            ensure!(
                delta.parent == after && delta.sequence == sequence,
                "cursor belongs to another fork"
            );
            out.write_all(MAGIC)?;
            put(&mut out, &serde_json::to_vec(&delta)?)?;
            let mut remaining = distance;
            let policy_root = tip.tree;
            let mut current = tip;
            while remaining > 0 {
                let power = level(remaining);
                put(&mut out, &serde_json::to_vec(&current)?)?;
                remaining -= 1_u64 << power;
                if remaining > 0 {
                    current = self.custody_head(current.ancestors[power])?;
                }
            }
            let mut tree = Tree::new(&self.db);
            let mut count = 0;
            // The authenticated delta selects the members. The granted tip supplies their ACLs.
            for m in &delta.delta {
                let (data, _) =
                    self.read_member_under(&mut tree, &delta, policy_root, m.id, "read")?;
                count += data.len();
                put(&mut out, &data)?;
            }
            io += tree.io;
            Ok((count, io))
        })
    }
    /// Verify a portable step against an independent target pin and rederive all index entries.
    pub fn custody_import_step(&self, target: Id, mut input: impl Read) -> Result<Admission> {
        let mut magic = [0; 4];
        input.read_exact(&mut magic)?;
        ensure!(&magic == MAGIC, "invalid paged frame");
        let (delta, next) = header(&mut input)?;
        verify_path(target, &delta, next, |_| header(&mut input))?;
        let mut items = Vec::new();
        let mut remaining = if delta.delta.len() == 1 {
            crate::wire::MAX_BYTES
        } else {
            BATCH_BYTES
        };
        for member in &delta.delta {
            let bytes = take(&mut input, remaining)?;
            remaining -= bytes.len();
            items.push(Item {
                member: member.clone(),
                bytes,
            });
        }
        ensure!(input.read(&mut [0])? == 0, "trailing frame data");
        self.admit_verified_delta(target, next, &delta, &items)
    }

    pub(super) fn admit_verified_delta(
        &self,
        target: Id,
        next: Id,
        delta: &Head,
        items: &[Item],
    ) -> Result<Admission> {
        let tx = rusqlite::Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let result = self.admit_batch(
            &delta.source,
            delta.parent,
            items,
            &delta.origins,
            delta.bootstrap,
        )?;
        ensure!(result.head == next, "derived inventory or coverage differs");
        self.register_snapshot(&delta.manifest()?, true)?;
        tx.execute(
            "INSERT INTO custody_pull_cursors VALUES(?,?,?) ON CONFLICT(target)
             DO UPDATE SET cursor=excluded.cursor,sequence=excluded.sequence WHERE excluded.sequence>sequence",
            params![target.to_string(), next.to_string(),i64::try_from(delta.sequence)?],
        )?;
        tx.commit()?;
        Ok(result)
    }
}

impl Archive {
    /// The receiver requests only missing ancestry descriptors. Member buffers move directly
    /// from the source read transaction into the bounded admission batch.
    pub(super) fn direct_step(
        &self,
        origin: &Archive,
        target: Id,
        tip: &Head,
        after: Option<Id>,
        sequence: u64,
        cache: &mut ProofCache,
    ) -> Result<(Admission, usize, usize, PageIo)> {
        let (next, delta, items, payload, descriptors, io) = origin.with_read_view(|| {
            let mut io = origin.permitted_pull(target, tip)?;
            let next = origin.ancestor(
                target,
                tip.sequence
                    .checked_sub(sequence)
                    .context("invalid cursor")?,
            )?;
            let mut descriptors = 0;
            let (delta, _) = cache.fetch(origin, next, &mut descriptors)?;
            ensure!(
                delta.parent == after && delta.sequence == sequence,
                "cursor belongs to another fork"
            );
            verify_path(target, &delta, next, |id| {
                cache.fetch(origin, id, &mut descriptors)
            })?;
            let mut tree = Tree::new(&origin.db);
            let mut payload = 0usize;
            let mut items = Vec::with_capacity(delta.delta.len());
            for member in &delta.delta {
                let (bytes, _) =
                    origin.read_member_under(&mut tree, &delta, tip.tree, member.id, "read")?;
                payload = payload
                    .checked_add(bytes.len())
                    .context("payload overflow")?;
                ensure!(
                    payload
                        <= if delta.delta.len() == 1 {
                            crate::wire::MAX_BYTES
                        } else {
                            BATCH_BYTES
                        },
                    "delta exceeds byte budget"
                );
                descriptors += 4;
                items.push(Item {
                    member: member.clone(),
                    bytes,
                });
            }
            io += tree.io;
            Ok((next, delta, items, payload, descriptors, io))
        })?;
        Ok((
            self.admit_verified_delta(target, next, &delta, &items)?,
            payload,
            descriptors,
            io,
        ))
    }
}
