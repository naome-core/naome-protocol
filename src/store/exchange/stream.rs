//! The streaming exchange carries the exact inventory once, followed by
//! ordered packets and a mandatory null terminator. Missing packets are
//! withheld objects, as in the original exchange format.
use super::{Archive, Packet, Snapshot, add_packet, inventory};
use crate::{
    store::snapshot::save_imported_head,
    wire::{Id, MAX_BYTES},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read, Write};

const FORMAT: &str = "naome-archive-stream";
const ARCHIVED_STREAM: &str = "rep-exchange-stream-1";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Witness {
    page: Id,
    data: inventory::Page,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum StreamRow {
    Witness(Witness),
    Packet(Packet),
    End(()),
}
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
        if snapshot.manifest.is_none() {
            let head = self.convert_archived(snapshot.id()?, snapshot)?;
            return self.export_stream_head(head, purpose, &mut output);
        }
        self.write_stream(snapshot.manifest()?, Some(snapshot), purpose, &mut output)
    }
    /// Ordinary read disclosure visits bounded inventory paths. Training additionally checks
    /// the selected scientific evidence closure for each candidate.
    pub fn export_stream_head(
        &self,
        head: Id,
        purpose: &str,
        output: &mut dyn Write,
    ) -> Result<usize> {
        let head = self.current_head_id(head)?;
        let manifest = self.active_manifest(head)?;
        let view = if purpose == "train" {
            Some(self.head(head)?)
        } else {
            None
        };
        self.write_stream(&manifest, view.as_ref(), purpose, output)
    }
    fn write_stream(
        &self,
        manifest: &super::Manifest,
        view: Option<&Snapshot>,
        purpose: &str,
        output: &mut dyn Write,
    ) -> Result<usize> {
        use super::super::custody::tree::Tree;
        self.with_read_view(|| {
            serde_json::to_writer(
                &mut *output,
                &Header {
                    format: FORMAT.to_owned(),
                    head: manifest.id()?,
                    snapshot: manifest,
                },
            )?;
            output.write_all(b"\n")?;
            let output = std::cell::RefCell::new(output);
            let mut pages = Tree::new(&self.db);
            let mut access = Tree::new(&self.db);
            let mut count = 0;
            let emit = |id, tree: &mut Tree<'_>| -> Result<_> {
                let data = tree.read(id)?;
                let mut writer = output.borrow_mut();
                serde_json::to_writer(
                    &mut **writer,
                    &Witness {
                        page: id,
                        data: data.clone(),
                    },
                )?;
                writer.write_all(b"\n")?;
                Ok(data)
            };
            manifest.walk(
                |id| emit(id, &mut pages),
                |id| {
                    if purpose == "train"
                        && let Some(view) = view
                        && !self
                            .explain(view, &[id], purpose, view.objects.len(), 48)
                            .frontier
                            .is_empty()
                    {
                        return Ok(());
                    }
                    let resolved = if let Some(view) = view {
                        self.resolve_with(view, id, purpose == "read", |acl| {
                            crate::profile::allows(acl, purpose)
                        })
                    } else {
                        self.resolve_inventory_member(manifest, id, purpose, &mut access)
                    };
                    let Ok((bytes, access)) = resolved else {
                        return Ok(());
                    };
                    if let Some(linked) = super::Manifest::from_packet(id, &bytes) {
                        // A missing linked horizon is evidence of an incomplete replay. Send
                        // it only when every page is available, without retaining its membership.
                        let mut tree = Tree::new(&self.db);
                        if linked.walk(|page| tree.read(page), |_| Ok(())).is_ok() {
                            linked.walk(|page| emit(page, &mut tree), |_| Ok(()))?
                        }
                    }
                    let mut writer = output.borrow_mut();
                    serde_json::to_writer(
                        &mut **writer,
                        &Packet {
                            id,
                            access,
                            hex: hex::encode(bytes),
                        },
                    )?;
                    writer.write_all(b"\n")?;
                    count += 1;
                    Ok(())
                },
            )?;
            output.into_inner().write_all(b"null\n")?;
            Ok(count)
        })
    }
    /// Verify a stream incrementally and commit the entire inventory atomically.
    pub fn import_stream(&mut self, input: impl BufRead) -> Result<Id> {
        self.import_stream_with_limit(input, 256 * 1024 * 1024)
    }

    /// Limit all incoming wire bytes, including witnesses, before committing any part.
    pub fn import_stream_with_limit(&mut self, input: impl BufRead, max_bytes: u64) -> Result<Id> {
        let mut input = input.take(max_bytes);
        let mut line = Vec::new();
        read_line(&mut input, &mut line, MAX_HEADER)?;
        let incoming: Header<super::super::snapshot::StoredHeader> = serde_json::from_slice(&line)?;
        let header = Header {
            format: incoming.format,
            head: incoming.head,
            snapshot: incoming.snapshot.into_snapshot(),
        };
        ensure!(
            (header.format == FORMAT
                && header.snapshot.manifest.is_some()
                && line.len() <= 32 * 1024)
                || (header.format == ARCHIVED_STREAM && header.snapshot.manifest.is_none()),
            "unsupported exchange stream format"
        );
        ensure!(header.snapshot.id()? == header.head, "invalid bundle head");
        // Retain the allocation for the most recent packet, never the stream.
        let mut bytes = Vec::new();
        let mut previous = None;
        let tx = self.db.unchecked_transaction()?;
        let witnesses = super::Witnesses::begin(&tx)?;
        let mut inventory = super::super::custody::tree::Tree::new(&tx);
        loop {
            read_line(&mut input, &mut line, MAX_PACKET)?;
            let packet = match serde_json::from_slice::<StreamRow>(&line)? {
                StreamRow::End(()) => break,
                StreamRow::Packet(p) => p,
                StreamRow::Witness(w) => {
                    ensure!(
                        header.format == FORMAT,
                        "unexpected page in historical stream"
                    );
                    witnesses.install(w.page, &w.data)?;
                    continue;
                }
            };
            ensure!(
                previous.is_none_or(|id| id < packet.id)
                    && if let Some(m) = &header.snapshot.manifest {
                        inventory.get(m.inventory, &packet.id.to_string())? == Some(packet.id)
                    } else {
                        header.snapshot.objects.contains(&packet.id)
                    },
                "unordered, duplicate or unexpected exchange packet"
            );
            add_packet(&tx, &packet, &mut bytes)?;
            witnesses.link(packet.id, &bytes)?;
            previous = Some(packet.id);
        }
        ensure!(
            input.fill_buf()?.is_empty(),
            "trailing data in exchange stream"
        );
        ensure!(
            input.into_inner().fill_buf()?.is_empty(),
            "stream exceeds receiver byte limit"
        );
        if let Some(m) = &header.snapshot.manifest {
            m.verify(header.head)?;
            witnesses.walk(m)?;
        } else {
            header.snapshot.verify_head(header.head)?
        }

        witnesses.finish()?;
        let head = if header.snapshot.manifest.is_none() {
            if self.current_head_id(header.head)? == header.head {
                save_imported_head(&tx, header.head, &header.snapshot)?;
            }
            self.convert_archived(header.head, &header.snapshot)?
        } else {
            save_imported_head(&tx, header.head, &header.snapshot)?;
            header.head
        };
        tx.commit()?;
        Ok(head)
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
