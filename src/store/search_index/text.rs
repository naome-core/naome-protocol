use super::{
    Archive, Passage, SearchLocation, Snapshot,
    lexical::{PASSAGE_BYTES, has_oversized_word, ranges},
};
use crate::{profile::Document, wire::Id};
use anyhow::{Context, Result, ensure};
use std::{io, ops::Range};

pub(super) const MAX_TEXT_BYTES: u64 = 64 * 1024 * 1024;
pub(super) const TEXT_WINDOW: usize = 64 * 1024;

/// Retain one resolved chunk while reading the same snapshot at one archive revision.
#[derive(Default)]
pub(super) struct VerifiedTextChunk {
    id: Option<Id>,
    bytes: Vec<u8>,
}

/// Validate UTF-8 across chunk boundaries without retaining the payload.
#[derive(Default)]
pub(super) struct Utf8Check {
    tail: [u8; 4],
    len: usize,
}

impl Utf8Check {
    pub(super) fn finish(&self) -> Result<()> {
        ensure!(self.len == 0, "incomplete UTF-8 sequence");
        Ok(())
    }
}

impl io::Write for Utf8Check {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let mut bytes = input;
        while self.len != 0 && !bytes.is_empty() {
            self.tail[self.len] = bytes[0];
            self.len += 1;
            bytes = &bytes[1..];
            match std::str::from_utf8(&self.tail[..self.len]) {
                Ok(_) => self.len = 0,
                Err(error) if error.error_len().is_none() => {}
                Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            }
        }
        if let Err(error) = std::str::from_utf8(bytes) {
            if error.error_len().is_some() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, error));
            }
            let tail = &bytes[error.valid_up_to()..];
            self.tail[..tail.len()].copy_from_slice(tail);
            self.len = tail.len();
        }
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn payload_passages(text: &str, offset: usize) -> impl Iterator<Item = Passage> {
    ranges(text).map(move |range| Passage {
        text: text[range.clone()].to_owned(),
        location: SearchLocation::Payload {
            start: offset + range.start,
            end: offset + range.end,
        },
        priority: 0,
        semantic: true,
    })
}

impl Archive {
    pub(super) fn index_text_window(
        &self,
        snapshot: &Snapshot,
        object: Id,
        cursor: usize,
        length: usize,
        chunk: &mut VerifiedTextChunk,
    ) -> Result<(Vec<Passage>, usize, bool)> {
        let document = self.read(snapshot, object, "read")?;
        let end = (cursor + TEXT_WINDOW).min(length);
        let bytes = chunk.read_span(self, snapshot, &document, cursor..end)?;
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(error) if end < length && error.error_len().is_none() => {
                std::str::from_utf8(&bytes[..error.valid_up_to()])?
            }
            Err(error) => return Err(error.into()),
        };
        // Carry the final portion into the next window. A word starting
        // here can need more bytes than this read contains.
        let cutoff = cursor
            + if end < length {
                text.len()
                    .saturating_sub(crate::search::MAX_TEXT_QUERY_BYTES + PASSAGE_BYTES)
            } else {
                text.len()
            };
        let mut next = cursor + text.len();
        let mut passages = Vec::new();
        for passage in payload_passages(text, cursor) {
            let SearchLocation::Payload { start, .. } = passage.location else {
                unreachable!("payload passages have payload locations");
            };
            if start >= cutoff {
                next = start;
                break;
            }
            passages.push(passage);
        }
        ensure!(next > cursor, "text index did not advance");
        Ok((passages, next, has_oversized_word(text)))
    }
}

impl VerifiedTextChunk {
    /// Index entries refer to a descriptor and chunks already verified through
    /// materialize. Resolve overlapping chunks on first use at this revision.
    pub(super) fn read_span(
        &mut self,
        archive: &Archive,
        snapshot: &Snapshot,
        document: &Document,
        range: Range<usize>,
    ) -> Result<Vec<u8>> {
        ensure!(
            range.start < range.end && range.end as u64 <= document.field::<u64>("length")?,
            "text selector outside the payload"
        );
        let first = document.inputs.partition_point(|link| {
            link.qualifier
                .parse::<usize>()
                .is_ok_and(|offset| offset <= range.start)
        });
        let first = first.checked_sub(1).context("missing text chunk")?;
        let mut bytes = Vec::with_capacity(range.len());
        for link in &document.inputs[first..] {
            let offset: usize = link.qualifier.parse()?;
            if offset >= range.end {
                break;
            }
            if self.id != Some(link.target) {
                *self = Self::default();
                self.bytes = archive.resolve(snapshot, link.target, "read")?;
                self.id = Some(link.target);
            }
            let start = range.start.saturating_sub(offset);
            let end = (range.end - offset).min(self.bytes.len());
            bytes.extend_from_slice(
                self.bytes
                    .get(start..end)
                    .context("invalid text chunk range")?,
            );
        }
        ensure!(bytes.len() == range.len(), "incomplete text range");
        Ok(bytes)
    }
}
