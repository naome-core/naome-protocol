use crate::{
    profile::{Document, Kind, PUBLIC, allows, link},
    store::{Archive, ObjectAvailability, Snapshot},
    wire::{Format, Id, MAX_BYTES},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Transaction, TransactionBehavior};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

pub(crate) const CAPTURED_EVIDENCE_SCOPE: &str = "captured-evidence-1";

pub struct CaptureContext<'a> {
    pub target: Id,
    pub note: Option<&'a str>,
    pub access: Option<Id>,
}

#[derive(Serialize)]
pub struct LinkedCapture {
    pub descriptor: Id,
    pub occurrence: Id,
}

/// Fill a fixed-size logical block across interrupted or short underlying reads.
pub(crate) fn read_block(source: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut count = 0;
    while count < buffer.len() {
        match source.read(&mut buffer[count..]) {
            Ok(0) => break,
            Ok(n) => count += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(count)
}

impl Archive {
    /// Stream an exact payload into an ordered chunk descriptor. A failed import rolls back every new object.
    pub fn capture(
        &self,
        source: impl Read,
        media: &str,
        chunk_size: usize,
        access: Id,
        f: Format,
    ) -> Result<Id> {
        let tx = self.db.unchecked_transaction()?;
        let id = self.capture_payload(source, media, chunk_size, access, f)?;
        tx.commit()?;
        Ok(id)
    }

    /// Capture bytes and record their use in local research in the same transaction.
    pub fn capture_for(
        &self,
        source: impl Read,
        media: &str,
        chunk_size: usize,
        context: CaptureContext<'_>,
        f: Format,
    ) -> Result<LinkedCapture> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let mut target_access = Id::blob(PUBLIC);
        let document = self.read_with(context.target, |id| {
            let (bytes, acl) = self.raw(id)?.context("research context unavailable")?;
            self.check_access(acl, |policy| allows(policy, "read"))?;
            if id == context.target {
                target_access = acl;
            }
            Ok(bytes)
        })?;
        ensure!(
            matches!(
                document.kind,
                Kind::Question | Kind::Plan | Kind::Occurrence | Kind::Derivation
            ),
            "capture context must be a question, plan, occurrence or derivation"
        );
        let access = context.access.unwrap_or(target_access);
        let public = Id::blob(PUBLIC);
        let occurrence_access = if target_access == public {
            access
        } else {
            ensure!(
                access == public || access == target_access,
                "incompatible research context and payload access policies"
            );
            target_access
        };
        self.check_access(access, |policy| allows(policy, "read"))?;
        let descriptor = self.capture_payload(source, media, chunk_size, access, f)?;
        let fields = match context.note {
            Some(note) => json!({"note":note}),
            None => json!({}),
        };
        let occurrence = self.document(
            &Document::new(
                Kind::Occurrence,
                &document.subject,
                CAPTURED_EVIDENCE_SCOPE,
                fields,
                vec![link("context", context.target), link("payload", descriptor)],
            )?,
            f,
            occurrence_access,
        )?;
        tx.commit()?;
        Ok(LinkedCapture {
            descriptor,
            occurrence,
        })
    }

    fn capture_payload(
        &self,
        mut source: impl Read,
        media: &str,
        chunk_size: usize,
        access: Id,
        f: Format,
    ) -> Result<Id> {
        ensure!(
            !media.is_empty() && (1..=MAX_BYTES).contains(&chunk_size),
            "invalid representation or chunk size"
        );
        let mut buf = vec![0; chunk_size];
        let mut digest = Sha256::new();
        let mut offset = 0u64;
        let mut chunks = Vec::new();
        loop {
            let count = read_block(&mut source, &mut buf)?;
            if count == 0 {
                break;
            }
            ensure!(
                chunks.len() < 16_384,
                "chunk manifest limit; use larger chunks"
            );
            let chunk = &buf[..count];
            let (id, availability) = self.store_blob(chunk, access)?;
            ensure!(
                availability == ObjectAvailability::Available,
                "payload contains a locally withdrawn chunk {id}"
            );
            digest.update(chunk);
            let mut e = link("chunk", id);
            e.qualifier = offset.to_string();
            chunks.push(e);
            offset = offset
                .checked_add(count as u64)
                .context("payload length overflow")?;
            if count < chunk_size {
                break;
            }
        }
        let d = Document::new(
            Kind::Descriptor,
            media,
            "ordered-chunks-1",
            json!({"algorithm":"SHA-256","sha256":hex::encode(digest.finalize()),"length":offset,"media":media}),
            chunks,
        )?;
        self.document(&d, f, access)
    }
    /// Write into an unpublished staging destination. Success certifies the complete sequence.
    pub fn materialize(
        &self,
        s: &Snapshot,
        id: Id,
        purpose: &str,
        budget: u64,
        output: impl Write,
    ) -> Result<u64> {
        self.read(s, id, purpose)?
            .materialize(budget, output, |chunk| self.resolve(s, chunk, purpose))
    }

    #[cfg(feature = "cli")]
    pub(crate) fn materialize_at(
        &self,
        head: Id,
        id: Id,
        budget: u64,
        output: impl Write,
    ) -> Result<u64> {
        let (mut inventory, descriptor) = self.with_read_view(|| {
            let mut inventory = self.inventory_reader(head)?;
            let descriptor = inventory.read(id)?;
            Ok((inventory, descriptor))
        })?;
        // Output I/O can take arbitrarily long. Release each read view before writing.
        descriptor.materialize(budget, output, |chunk| {
            self.with_read_view(|| Ok(inventory.resolve(chunk)?.0))
        })
    }
}

impl Document {
    pub(crate) fn is_chunk_descriptor(&self) -> bool {
        self.kind == Kind::Descriptor && self.scope == "ordered-chunks-1"
    }

    pub(crate) fn materialize(
        &self,
        budget: u64,
        mut output: impl Write,
        mut resolve: impl FnMut(Id) -> Result<Vec<u8>>,
    ) -> Result<u64> {
        ensure!(
            self.is_chunk_descriptor()
                && self.field::<String>("algorithm")? == "SHA-256"
                && self.field::<String>("media")? == self.subject,
            "unknown descriptor"
        );
        let length: u64 = self.field("length")?;
        ensure!(length <= budget, "payload budget exhausted");
        let mut digest = Sha256::new();
        let mut offset = 0u64;
        for e in &self.inputs {
            ensure!(
                e.role == "chunk"
                    && e.qualifier == offset.to_string()
                    && e.target.format().is_none(),
                "invalid position or chunk reference"
            );
            let b = resolve(e.target)?;
            ensure!(!b.is_empty(), "empty chunk");
            offset = offset
                .checked_add(b.len() as u64)
                .context("payload length overflow")?;
            ensure!(offset <= length, "payload larger than declared length");
            digest.update(&b);
            output.write_all(&b)?;
        }
        ensure!(
            offset == length && hex::encode(digest.finalize()) == self.field::<String>("sha256")?,
            "payload digest or length disagrees"
        );
        Ok(offset)
    }
}

#[cfg(all(test, feature = "cli"))]
mod tests {
    use super::*;
    use std::{io, time::Duration};

    #[test]
    fn recovery_releases_the_read_view_before_writing_a_chunk() -> Result<()> {
        struct WithdrawDuringWrite<'a> {
            archive: &'a Archive,
            chunk: Id,
            wrote: bool,
        }

        impl Write for WithdrawDuringWrite<'_> {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                if !self.wrote {
                    self.archive
                        .withdraw(self.chunk)
                        .map_err(io::Error::other)?;
                    self.wrote = true;
                }
                Ok(buffer.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("archive.sqlite");
        let writer = Archive::open(&path)?;
        let descriptor = writer.capture(
            &b"firstlast"[..],
            "application/octet-stream",
            5,
            Id::blob(PUBLIC),
            Format::A,
        )?;
        let head = writer.snapshot_id("slow destination", Default::default())?;
        writer.db.busy_timeout(Duration::from_millis(5))?;
        let reader = Archive::open_for_read(&path)?;
        let mut sink = WithdrawDuringWrite {
            archive: &writer,
            chunk: Id::blob(b"last"),
            wrote: false,
        };

        let error = reader
            .materialize_at(head, descriptor, 9, &mut sink)
            .unwrap_err();
        assert!(sink.wrote, "{error:#}");
        assert!(error.to_string().contains("unavailable"));
        Ok(())
    }
}
