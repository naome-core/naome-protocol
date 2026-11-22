use super::{
    Archive, Passage, SearchLocation, Snapshot,
    extract::{PASSAGE_BYTES, ranges},
};
use crate::{profile::Document, wire::Id};
use anyhow::{Context, Result, ensure};
use std::{io, ops::Range};

pub(super) const MAX_TEXT_BYTES: u64 = 64 * 1024 * 1024;
pub(super) const TEXT_WINDOW: usize = 64 * 1024;

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
    ) -> Result<(Vec<Passage>, usize)> {
        let document = self.read(snapshot, object, "read")?;
        let end = (cursor + TEXT_WINDOW).min(length);
        let bytes = self.read_verified_text_span(snapshot, &document, cursor..end)?;
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(error) if end < length && error.error_len().is_none() => {
                std::str::from_utf8(&bytes[..error.valid_up_to()])?
            }
            Err(error) => return Err(error.into()),
        };
        let mut passages: Vec<_> = payload_passages(text, cursor).collect();
        // Retain the last passage for the next window so phrases at the
        // window boundary can be tokenized with their continuation.
        let mut next = cursor + text.len();
        if end < length
            && let Some(Passage {
                location: SearchLocation::Payload { start, end },
                ..
            }) = passages.last()
            && *start > cursor
            && next - *end < PASSAGE_BYTES
        {
            next = *start;
            passages.pop();
        }
        ensure!(next > cursor, "text index did not advance");
        Ok((passages, next))
    }

    /// Index entries refer to a descriptor and chunks already verified through
    /// materialize. Resolve only overlapping chunks, with current permissions.
    pub(super) fn read_verified_text_span(
        &self,
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
            let chunk = self.resolve(snapshot, link.target, "read")?;
            let start = range.start.saturating_sub(offset);
            let end = (range.end - offset).min(chunk.len());
            bytes.extend_from_slice(chunk.get(start..end).context("invalid text chunk range")?);
        }
        ensure!(bytes.len() == range.len(), "incomplete text range");
        Ok(bytes)
    }
}
