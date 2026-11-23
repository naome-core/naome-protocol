//! Offline inventory packets and atomic import into a custody archive.
use super::{Archive, Snapshot, insert, save_imported_head, validate};
use crate::{
    profile::allows,
    wire::{Id, MAX_BYTES},
};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::{BufReader, Read},
};

mod stream;

#[cfg(test)]
mod tests;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Packet {
    pub id: Id,
    pub access: Id,
    pub hex: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub snapshot: Snapshot,
    pub head: Id,
    pub packets: Vec<Packet>,
    pub omitted: BTreeSet<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<Id>,
}
impl Bundle {
    fn verify_inventory(&self) -> Result<()> {
        verify_head(self.head, &self.snapshot)?;
        let objects = &self.snapshot.objects;
        ensure!(
            objects.len().checked_sub(self.omitted.len()) == Some(self.packets.len()),
            "inventory mismatch"
        );
        let expected = objects.difference(&self.omitted).copied();
        let packet_ids = self.packets.iter().map(|packet| packet.id);
        if !expected.clone().eq(packet_ids.clone()) {
            // Legacy bundles may present packets in any order.
            let mut ordered: Vec<_> = packet_ids.collect();
            ordered.sort_unstable();
            ensure!(expected.eq(ordered), "inventory mismatch");
        }
        Ok(())
    }

    /// Decode one offline exchange packet within the import byte budget.
    pub fn read(reader: impl Read) -> Result<Self> {
        Self::read_bounded(reader, 256 * 1024 * 1024)
    }

    fn read_bounded(reader: impl Read, limit: u64) -> Result<Self> {
        let mut limited = reader.take(limit + 1);
        let bundle = serde_json::from_reader(BufReader::new(&mut limited));
        // The parser checks through EOF, including trailing whitespace. Reading
        // the sentinel byte distinguishes a valid file at the limit from a prefix.
        ensure!(limited.limit() > 0, "exchange byte limit");
        Ok(bundle?)
    }
}

fn decode_packet_hex(encoded: &str, decoded: &mut Vec<u8>) -> Result<(), hex::FromHexError> {
    const NIBBLES: [u8; 256] = {
        let mut values = [u8::MAX; 256];
        let lower = b"0123456789abcdef";
        let upper = b"0123456789ABCDEF";
        let mut n = 0;
        while n < lower.len() {
            values[lower[n] as usize] = n as u8;
            values[upper[n] as usize] = n as u8;
            n += 1;
        }
        values
    };

    let (pairs, remainder) = encoded.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(hex::FromHexError::OddLength);
    }

    // Accumulate invalid-nibble sentinels while decoding. Only malformed input
    // needs a second pass to recover the precise error and byte offset.
    let mut nibbles = 0;
    decoded.clear();
    decoded.extend(pairs.iter().map(|&[high, low]| {
        let high = NIBBLES[high as usize];
        let low = NIBBLES[low as usize];
        nibbles |= high | low;
        (high << 4) | low
    }));
    if nibbles > 0x0f {
        hex::decode_to_slice(encoded, decoded)?;
    }
    Ok(())
}

impl Archive {
    fn packet(&self, s: &Snapshot, id: Id, purpose: &str) -> Result<Packet> {
        let (bytes, access) = self.resolve_with(s, id, |policy| allows(policy, purpose))?;
        Ok(Packet {
            id,
            access,
            hex: hex::encode(bytes),
        })
    }

    pub fn export(&self, s: &Snapshot, purpose: &str) -> Result<Bundle> {
        let packet = self.export_page(s, purpose, None, usize::MAX)?;
        ensure!(
            packet.continuation.is_none(),
            "exchange exceeds single-packet size; use paged export"
        );
        Ok(packet)
    }
    pub fn export_page(
        &self,
        s: &Snapshot,
        purpose: &str,
        after: Option<Id>,
        limit: usize,
    ) -> Result<Bundle> {
        self.with_read_view(|| self.collect_page(s, purpose, after, limit))
    }

    fn collect_page(
        &self,
        s: &Snapshot,
        purpose: &str,
        after: Option<Id>,
        limit: usize,
    ) -> Result<Bundle> {
        ensure!(limit > 0, "positive export page limit required");
        let (head, mut used) = {
            let manifest = serde_json::to_vec(s)?;
            let used = manifest
                .len()
                .checked_mul(3)
                .context("inventory too large")?;
            ensure!(
                used < 200 * 1024 * 1024,
                "inventory exceeds exchange size limit"
            );
            (Id::blob(&manifest), used)
        };
        let mut last = None;
        let mut continuation = None;
        let mut examined = 0;
        let mut packets = Vec::new();
        let mut omitted = BTreeSet::new();
        for &id in &s.objects {
            if after.is_some_and(|a| id <= a) || continuation.is_some() {
                omitted.insert(id);
                continue;
            }
            let reserved = self.size(id)?.saturating_mul(2).saturating_add(512);
            if examined == limit || used.saturating_add(reserved) > 250 * 1024 * 1024 {
                continuation = Some(last.context("object exceeds available packet capacity")?);
                omitted.insert(id);
                continue;
            }
            last = Some(id);
            examined += 1;
            used += reserved;
            if let Some(packet) = self.exchange_packet(s, id, purpose) {
                packets.push(packet);
            } else {
                omitted.insert(id);
            }
        }
        Ok(Bundle {
            head,
            snapshot: s.clone(),
            packets,
            omitted,
            continuation,
        })
    }
    fn exchange_packet(&self, s: &Snapshot, id: Id, purpose: &str) -> Option<Packet> {
        if purpose == "train"
            && !self
                .explain(s, &[id], purpose, s.objects.len(), 48)
                .frontier
                .is_empty()
        {
            return None;
        }
        self.packet(s, id, purpose).ok()
    }

    pub fn import(&mut self, b: &Bundle) -> Result<Id> {
        b.verify_inventory()?;
        let tx = self.db.transaction()?;
        let mut bytes = Vec::new();
        for p in &b.packets {
            add_packet(&tx, p, &mut bytes)?;
        }
        save_imported_head(&tx, b.head, &b.snapshot)?;
        tx.commit()?;
        Ok(b.head)
    }
}

fn verify_head(head: Id, snapshot: &Snapshot) -> Result<()> {
    ensure!(
        snapshot.version == 1 && !snapshot.source.is_empty() && snapshot.id()? == head,
        "invalid bundle head"
    );
    Ok(())
}

fn add_packet(db: &Connection, packet: &Packet, bytes: &mut Vec<u8>) -> Result<()> {
    ensure!(packet.hex.len() <= 2 * MAX_BYTES, "oversized packet");
    decode_packet_hex(&packet.hex, bytes)?;
    validate(packet.id, bytes, packet.access)?;
    insert(db, packet.id, bytes, packet.access)?;
    Ok(())
}
