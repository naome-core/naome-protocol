use crate::{
    profile::{Document, Kind, link},
    store::{Archive, ObjectAvailability, Snapshot},
    wire::{Format, Id, MAX_BYTES},
};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

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
        let tx = self.db.unchecked_transaction()?;
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
        let id = self.document(&d, f, access)?;
        tx.commit()?;
        Ok(id)
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
