//! The streaming exchange carries the exact inventory once, followed by
//! ordered packets and a mandatory null terminator. Missing packets are
//! withheld objects, as in the original exchange format.
use super::{Archive, Packet, Snapshot, add_packet, verify_head};
use crate::{
    store::save_imported_head,
    wire::{Id, MAX_BYTES},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read, Write};

const FORMAT: &str = "rep-exchange-stream-1";
const MAX_HEADER: usize = 64 * 1024 * 1024;
const MAX_PACKET: usize = 2 * MAX_BYTES + 512;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header<S> {
    format: String,
    head: Id,
    snapshot: S,
}

impl Archive {
    /// Write a snapshot once and emit its permitted packets incrementally.
    pub fn export_stream(
        &self,
        snapshot: &Snapshot,
        purpose: &str,
        mut output: impl Write,
    ) -> Result<usize> {
        self.with_read_view(|| {
            let head = snapshot.id()?;
            let header = Header {
                format: FORMAT.to_owned(),
                head,
                snapshot,
            };
            let header = serde_json::to_vec(&header)?;
            ensure!(
                header.len() < MAX_HEADER,
                "exchange stream header exceeds byte limit"
            );
            output.write_all(&header)?;
            output.write_all(b"\n")?;
            let mut count = 0;
            for &id in &snapshot.objects {
                if let Some(packet) = self.exchange_packet(snapshot, id, purpose) {
                    serde_json::to_writer(&mut output, &packet)?;
                    output.write_all(b"\n")?;
                    count += 1;
                }
            }
            output.write_all(b"null\n")?;
            Ok(count)
        })
    }

    /// Verify a stream incrementally and commit the entire inventory atomically.
    pub fn import_stream(&mut self, mut input: impl BufRead) -> Result<Id> {
        let mut line = Vec::new();
        read_line(&mut input, &mut line, MAX_HEADER)?;
        let header: Header<Snapshot> = serde_json::from_slice(&line)?;
        ensure!(
            header.format == FORMAT,
            "unsupported exchange stream format"
        );
        verify_head(header.head, &header.snapshot)?;
        // Retain the allocation for the most recent packet, never the stream.
        let mut bytes = Vec::new();
        let mut previous = None;
        let tx = self.db.transaction()?;
        loop {
            read_line(&mut input, &mut line, MAX_PACKET)?;
            let Some(packet) = serde_json::from_slice::<Option<Packet>>(&line)? else {
                break;
            };
            ensure!(
                previous.is_none_or(|id| id < packet.id)
                    && header.snapshot.objects.contains(&packet.id),
                "unordered, duplicate or unexpected exchange packet"
            );
            add_packet(&tx, &packet, &mut bytes)?;
            previous = Some(packet.id);
        }
        ensure!(
            input.fill_buf()?.is_empty(),
            "trailing data in exchange stream"
        );
        save_imported_head(&tx, header.head, &header.snapshot)?;
        tx.commit()?;
        Ok(header.head)
    }
}

fn read_line(input: &mut impl BufRead, line: &mut Vec<u8>, limit: usize) -> Result<()> {
    line.clear();
    input.take((limit + 1) as u64).read_until(b'\n', line)?;
    ensure!(
        line.len() <= limit && line.last() == Some(&b'\n'),
        "missing, truncated or oversized exchange stream line"
    );
    Ok(())
}
