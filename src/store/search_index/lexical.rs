//! Unicode normalization, tokens and passage boundaries shared by indexing and lookup.
use crate::search::MAX_TEXT_QUERY_BYTES;
use caseless::Caseless;
use std::{collections::BTreeSet, ops::Range};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

pub(super) const PASSAGE_BYTES: usize = 768;

pub(super) fn normalize(text: &str) -> String {
    text.nfkc().default_case_fold().nfkc().collect()
}

pub(super) fn terms(normalized: &str) -> BTreeSet<&str> {
    // UAX #29 retains scripts that do not use whitespace. Add adjacent Han
    // tokens as an extra signal so a common single character cannot dominate.
    let words: Vec<_> = normalized.unicode_word_indices().collect();
    let mut result: BTreeSet<_> = words.iter().map(|&(_, word)| word).collect();
    for pair in words.windows(2) {
        let [(start, first), (next, second)] = pair else {
            continue;
        };
        if start + first.len() == *next {
            result.insert(&normalized[*start..next + second.len()]);
        }
    }
    result
}

pub(super) fn ranges(text: &str) -> impl Iterator<Item = Range<usize>> {
    let mut start = 0;
    std::iter::from_fn(move || {
        let remaining = text.get(start..)?.trim_start();
        if remaining.is_empty() {
            return None;
        }
        start = text.len() - remaining.len();
        let mut width = remaining.len().min(PASSAGE_BYTES);
        while !remaining.is_char_boundary(width) {
            width -= 1;
        }
        if width < remaining.len() {
            if let Some(boundary) = remaining[..width].rfind(char::is_whitespace)
                && boundary >= width / 2
            {
                width = boundary;
            } else {
                // Look past the preferred cut to keep a searchable word intact.
                let mut lookahead = (MAX_TEXT_QUERY_BYTES + PASSAGE_BYTES).min(remaining.len());
                while !remaining.is_char_boundary(lookahead) {
                    lookahead -= 1;
                }
                let mut boundaries = remaining[..lookahead]
                    .split_word_bound_indices()
                    .skip(1)
                    .map(|(offset, _)| offset)
                    .peekable();
                let mut preceding = None;
                while let Some(offset) = boundaries.next_if(|&offset| offset <= width) {
                    preceding = Some(offset);
                }
                width = preceding
                    .or_else(|| boundaries.next())
                    .or_else(|| (lookahead == remaining.len()).then_some(lookahead))
                    .filter(|&offset| offset <= MAX_TEXT_QUERY_BYTES)
                    .unwrap_or(width);
            }
        }
        let end = start + remaining[..width].trim_end().len();
        let range = start..end;
        start += width;
        Some(range)
    })
}

pub(super) fn has_oversized_word(text: &str) -> bool {
    text.unicode_words()
        .any(|word| word.len() > MAX_TEXT_QUERY_BYTES)
}
