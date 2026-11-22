//! Offline inventory packets and atomic import into a custody archive.
use super::{Archive, Snapshot, insert, validate};
use crate::{
    profile::allows,
    wire::{Id, MAX_BYTES},
};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::{BufReader, Read},
};

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
        ensure!(limit > 0, "positive export page limit required");
        let mut used = serde_json::to_vec(s)?
            .len()
            .checked_mul(3)
            .context("inventory too large")?;
        ensure!(
            used < 200 * 1024 * 1024,
            "inventory exceeds exchange size limit"
        );
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
            if purpose == "train"
                && !self
                    .explain(s, &[id], purpose, s.objects.len(), 48)
                    .frontier
                    .is_empty()
            {
                omitted.insert(id);
                continue;
            }
            match self.packet(s, id, purpose) {
                Ok(packet) => packets.push(packet),
                Err(_) => {
                    omitted.insert(id);
                }
            }
        }
        Ok(Bundle {
            head: s.id()?,
            snapshot: s.clone(),
            packets,
            omitted,
            continuation,
        })
    }
    pub fn import(&mut self, b: &Bundle) -> Result<Id> {
        ensure!(
            b.snapshot.version == 1
                && !b.snapshot.source.is_empty()
                && b.head == b.snapshot.id()?,
            "invalid bundle head"
        );
        let tx = self.db.transaction()?;
        let mut ids = b.omitted.clone();
        for p in &b.packets {
            ensure!(
                ids.insert(p.id) && p.hex.len() <= 2 * MAX_BYTES,
                "duplicate or oversized packet"
            );
            let bytes = hex::decode(&p.hex)?;
            validate(p.id, &bytes, p.access)?;
            insert(&tx, p.id, &bytes, p.access)?
        }
        ensure!(ids == b.snapshot.objects, "inventory mismatch");
        tx.execute(
            "INSERT OR IGNORE INTO heads VALUES(?,?)",
            params![b.head.to_string(), serde_json::to_vec(&b.snapshot)?],
        )?;
        tx.commit()?;
        Ok(b.head)
    }
}
