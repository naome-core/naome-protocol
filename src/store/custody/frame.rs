//! A portable, length-delimited delta and an authenticated path from the externally pinned tip.
use super::*;
use std::io::{Read, Write};

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
    let h: Head = serde_json::from_slice(&bytes)?;
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
            for m in &delta.delta {
                let data = self.read_member(&mut tree, &delta, m.id, "read")?;
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
        let mut expected = target;
        let mut expected_seq = None;
        let mut hops = 0;
        while expected != next {
            hops += 1;
            ensure!(hops <= 64, "ancestry budget exhausted");
            let (proof, id) = header(&mut input)?;
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
        let tx = rusqlite::Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let result = self.admit_batch(
            &delta.source,
            delta.parent,
            &items,
            &delta.origins,
            delta.bootstrap,
        )?;
        ensure!(result.head == next, "derived inventory or coverage differs");
        self.register_snapshot(&delta.manifest()?, true)?;
        tx.commit()?;
        Ok(result)
    }
}
