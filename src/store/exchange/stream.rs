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
            let emit = |m: &super::Manifest, out: &mut dyn Write| -> Result<()> {
                let mut tree = Tree::new(&self.db);
                m.walk(
                    |id| {
                        let data = tree.read(id)?;
                        serde_json::to_writer(
                            &mut *out,
                            &Witness {
                                page: id,
                                data: data.clone(),
                            },
                        )?;
                        out.write_all(b"\n")?;
                        Ok(data)
                    },
                    |_| Ok(()),
                )
            };
            emit(manifest, output)?;
            let mut tree = Tree::new(&self.db);
            let mut count = 0;
            manifest.walk(
                |id| tree.read(id),
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
                        self.resolve_with(view, id, |acl| crate::profile::allows(acl, purpose))
                    } else {
                        self.resolve_member(manifest, id, purpose)
                    };
                    let Ok((bytes, access)) = resolved else {
                        return Ok(());
                    };
                    if id.format().is_none()
                        && bytes.len() <= 32768
                        && let Ok(linked) = serde_json::from_slice::<super::Manifest>(&bytes)
                    {
                        // A missing linked horizon is evidence of an incomplete replay. Send
                        // it only when every page is available, without retaining its membership.
                        let mut tree = Tree::new(&self.db);
                        if linked.walk(|page| tree.read(page), |_| Ok(())).is_ok() {
                            emit(&linked, output)?
                        }
                    }
                    serde_json::to_writer(
                        &mut *output,
                        &Packet {
                            id,
                            access,
                            hex: hex::encode(bytes),
                        },
                    )?;
                    output.write_all(b"\n")?;
                    count += 1;
                    Ok(())
                },
            )?;
            output.write_all(b"null\n")?;
            Ok(count)
        })
    }
    /// Verify a stream incrementally and commit the entire inventory atomically.
    pub fn import_stream(&mut self, mut input: impl BufRead) -> Result<Id> {
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
                    inventory::install(&tx, &[(w.page, w.data)].into())?;
                    continue;
                }
            };
            ensure!(
                previous.is_none_or(|id| id < packet.id)
                    && if let Some(m) = &header.snapshot.manifest {
                        super::super::custody::tree::Tree::new(&tx)
                            .get(m.inventory, &packet.id.to_string())?
                            == Some(packet.id)
                    } else {
                        header.snapshot.objects.contains(&packet.id)
                    },
                "unordered, duplicate or unexpected exchange packet"
            );
            add_packet(&tx, &packet, &mut bytes)?;
            previous = Some(packet.id);
        }
        ensure!(
            input.fill_buf()?.is_empty(),
            "trailing data in exchange stream"
        );
        if let Some(m) = &header.snapshot.manifest {
            m.verify(header.head)?;
            let mut tree = super::super::custody::tree::Tree::new(&tx);
            m.walk(|id| tree.read(id), |_| Ok(()))?;
        } else {
            header.snapshot.verify_head(header.head)?
        }

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
