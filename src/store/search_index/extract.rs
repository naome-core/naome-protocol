use super::{Dependencies, IndexedRecord, IndexedSection, Passage};
use crate::{
    profile::{Document, Kind, allows},
    search::{MAX_MEDIA_BYTES, MediaSpan, SearchLocation, media_type, native_media, text_media},
    store::{Archive, Snapshot},
    wire::Id,
};
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::{collections::BTreeSet, ops::Range};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

const MAX_PAYLOAD: u64 = 256 * 1024;
const MAX_FIELDS: usize = 64;
const MAX_FIELD_BYTES: usize = 4096;
const PASSAGE_BYTES: usize = 768;

pub(super) fn normalize(text: &str) -> String {
    text.nfkc().flat_map(char::to_lowercase).collect()
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
        if width < remaining.len()
            && let Some(boundary) = remaining[..width].rfind(char::is_whitespace)
            && boundary >= width / 2
        {
            width = boundary;
        }
        let end = start + remaining[..width].trim_end().len();
        let range = start..end;
        start += width;
        Some(range)
    })
}

fn add_field(
    passages: &mut Vec<Passage>,
    pointer: &str,
    text: &str,
    priority: i64,
    semantic: bool,
) {
    let mut end = text.len().min(MAX_FIELD_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    passages.extend(ranges(&text[..end]).map(|range| Passage {
        text: text[range.clone()].to_owned(),
        location: SearchLocation::Field {
            pointer: pointer.to_owned(),
            start: range.start,
            end: range.end,
        },
        priority,
        semantic,
    }));
}

fn field_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn collect_fields(
    value: &Value,
    pointer: &str,
    remaining: &mut usize,
    limited: &mut bool,
    semantic: bool,
    passages: &mut Vec<Passage>,
) {
    if *remaining == 0 || pointer.len() > 512 {
        *limited = true;
        return;
    }
    *remaining -= 1;
    if let Some(text) = field_text(value) {
        if text.parse::<Id>().is_err() {
            *limited |= text.len() > MAX_FIELD_BYTES;
            add_field(passages, pointer, &text, 0, semantic && value.is_string());
        }
        return;
    }
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                let key = key.replace('~', "~0").replace('/', "~1");
                collect_fields(
                    value,
                    &format!("{pointer}/{key}"),
                    remaining,
                    limited,
                    semantic,
                    passages,
                );
                if *remaining == 0 {
                    break;
                }
            }
        }
        Value::Array(values) => {
            for (i, value) in values.iter().enumerate() {
                collect_fields(
                    value,
                    &format!("{pointer}/{i}"),
                    remaining,
                    limited,
                    semantic,
                    passages,
                );
                if *remaining == 0 {
                    break;
                }
            }
        }
        _ => {}
    }
}

impl Archive {
    pub(super) fn search_sections(&self, snapshot: &Snapshot, id: Id) -> IndexedRecord {
        let mut dependencies = Dependencies::default();
        let Ok(document) =
            self.read_with(id, |id| self.search_source(snapshot, id, &mut dependencies))
        else {
            return IndexedRecord {
                document: None,
                sections: Vec::new(),
                missing: dependencies.missing,
                limited: true,
            };
        };

        let descriptor = document.kind == Kind::Descriptor && document.scope == "ordered-chunks-1";
        let mut passages = Vec::new();
        add_field(&mut passages, "/subject", &document.subject, 2, !descriptor);
        add_field(&mut passages, "/scope", &document.scope, 0, false);
        let mut remaining = MAX_FIELDS;
        let mut limited = [document.subject.len(), document.scope.len()]
            .into_iter()
            .any(|length| length > MAX_FIELD_BYTES);
        for (key, value) in &document.fields {
            if descriptor && matches!(key.as_str(), "sha256" | "algorithm") {
                continue;
            }
            let key = key.replace('~', "~0").replace('/', "~1");
            collect_fields(
                value,
                &format!("/fields/{key}"),
                &mut remaining,
                &mut limited,
                !descriptor,
                &mut passages,
            );
            if remaining == 0 {
                limited = true;
                break;
            }
        }
        let mut sections = vec![IndexedSection {
            sources: dependencies.available.clone(),
            passages,
        }];
        if descriptor {
            let media = media_type(&document.subject);
            if text_media(media) {
                let mut data = Vec::new();
                let payload = document
                    .materialize(MAX_PAYLOAD, &mut data, |id| {
                        self.search_source(snapshot, id, &mut dependencies)
                    })
                    .and_then(|_| Ok(String::from_utf8(data)?));
                match payload {
                    Ok(text) => {
                        let passages = ranges(&text)
                            .map(|range| Passage {
                                text: text[range.clone()].to_owned(),
                                location: SearchLocation::Payload {
                                    start: range.start,
                                    end: range.end,
                                },
                                priority: 0,
                                semantic: true,
                            })
                            .collect();
                        sections.push(IndexedSection {
                            sources: dependencies.available,
                            passages,
                        });
                    }
                    Err(_) => limited = true,
                }
            } else if native_media(media) {
                let verified = document.materialize(MAX_MEDIA_BYTES, std::io::sink(), |id| {
                    self.search_source(snapshot, id, &mut dependencies)
                });
                match verified {
                    Ok(length) if length > 0 => sections.push(IndexedSection {
                        sources: dependencies.available,
                        passages: vec![Passage {
                            text: String::new(),
                            location: SearchLocation::Media {
                                media_type: media.to_owned(),
                                span: MediaSpan::Whole,
                            },
                            priority: 0,
                            semantic: true,
                        }],
                    }),
                    _ => limited = true,
                }
            } else {
                limited = true;
            }
        }
        IndexedRecord {
            document: Some(document),
            sections,
            missing: dependencies.missing,
            limited,
        }
    }

    fn search_source(
        &self,
        snapshot: &Snapshot,
        id: Id,
        dependencies: &mut Dependencies,
    ) -> Result<Vec<u8>> {
        match self.resolve_with(snapshot, id, |policy| allows(policy, "read")) {
            Ok((bytes, access)) => {
                dependencies.available.extend([id, access]);
                Ok(bytes)
            }
            Err(error) => {
                if let Some(missing) = self.search_missing(snapshot, id)? {
                    dependencies.missing.insert(missing);
                }
                Err(error)
            }
        }
    }

    fn search_missing(&self, snapshot: &Snapshot, id: Id) -> Result<Option<Id>> {
        if !snapshot.objects.contains(&id) {
            return Ok(None);
        }
        let object: Option<(String, bool)> = self
            .db
            .query_row(
                "SELECT access,data IS NOT NULL FROM objects WHERE id=?",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((access, available)) = object else {
            return Ok(Some(id));
        };
        let access = access.parse()?;
        if !available || !snapshot.objects.contains(&access) {
            return Ok(None);
        }
        let known: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE id=?)",
            [access.to_string()],
            |row| row.get(0),
        )?;
        Ok((!known).then_some(access))
    }

    pub(super) fn search_snippet(
        &self,
        snapshot: &Snapshot,
        id: Id,
        document: &Document,
        location: &SearchLocation,
    ) -> Result<Option<String>> {
        Ok(match location {
            SearchLocation::Object | SearchLocation::Media { .. } => None,
            SearchLocation::Field {
                pointer,
                start,
                end,
            } => {
                let value = serde_json::to_value(document)?;
                value
                    .pointer(pointer)
                    .and_then(field_text)
                    .and_then(|text| text.get(*start..*end).map(str::to_owned))
            }
            SearchLocation::Payload { start, end } => {
                let mut bytes = Vec::new();
                self.materialize(snapshot, id, "read", MAX_PAYLOAD, &mut bytes)?;
                std::str::from_utf8(&bytes)?
                    .get(*start..*end)
                    .map(str::to_owned)
            }
        })
    }
}
