use super::{
    Dependencies, IndexedRecord, IndexedSection, Passage,
    lexical::{has_oversized_word, ranges},
    text::{MAX_TEXT_BYTES, Utf8Check, VerifiedTextChunk, payload_passages},
};
use crate::{
    profile::{Document, Kind, allows},
    search::{MAX_MEDIA_BYTES, MediaSpan, SearchLocation, media_type, native_media, text_media},
    store::{Archive, Snapshot},
    wire::Id,
};
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::collections::BTreeSet;

const MAX_PAYLOAD: u64 = 256 * 1024;
const MAX_FIELDS: usize = 64;
const MAX_FIELD_BYTES: usize = 4096;
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
            let named = pointer.rsplit('/').next().is_some_and(|name| {
                matches!(
                    name,
                    "title" | "name" | "code" | "identifier" | "accession" | "catalog_number"
                )
            });
            add_field(
                passages,
                pointer,
                &text,
                if named { 2 } else { 0 },
                semantic && value.is_string(),
            );
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
                cacheable: false,
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
            pending_text: None,
        }];
        let mut cacheable = true;
        if descriptor {
            let media = &document.subject;
            if text_media(media) {
                match self.search_text(snapshot, &document, &mut dependencies) {
                    Ok((section, incomplete)) => {
                        sections.push(section);
                        limited |= incomplete;
                    }
                    Err(_) => {
                        limited = true;
                        cacheable = false;
                    }
                }
            } else if native_media(media) {
                let verified = self.media_dependencies(snapshot, &document, &mut dependencies);
                match verified {
                    Ok(length) if length > 0 => sections.push(IndexedSection {
                        sources: dependencies.available,
                        passages: vec![Passage {
                            text: String::new(),
                            location: SearchLocation::Media {
                                media_type: media_type(media).into_owned(),
                                span: MediaSpan::Whole,
                            },
                            priority: 0,
                            semantic: true,
                        }],
                        pending_text: None,
                    }),
                    _ => {
                        limited = true;
                        cacheable = false;
                    }
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
            cacheable,
        }
    }

    fn search_text(
        &self,
        snapshot: &Snapshot,
        document: &Document,
        dependencies: &mut Dependencies,
    ) -> Result<(IndexedSection, bool)> {
        let length: u64 = document.field("length")?;
        let mut resolve = |id| self.search_source(snapshot, id, dependencies);
        let (passages, pending_text, limited) = if length <= MAX_PAYLOAD {
            let mut bytes = Vec::new();
            document.materialize(MAX_PAYLOAD, &mut bytes, &mut resolve)?;
            let text = std::str::from_utf8(&bytes)?;
            (
                payload_passages(text, 0).collect(),
                None,
                has_oversized_word(text),
            )
        } else {
            let mut utf8 = Utf8Check::default();
            document.materialize(MAX_TEXT_BYTES, &mut utf8, &mut resolve)?;
            utf8.finish()?;
            (Vec::new(), Some(length), false)
        };
        Ok((
            IndexedSection {
                sources: dependencies.available.clone(),
                passages,
                pending_text,
            },
            limited,
        ))
    }

    fn media_dependencies(
        &self,
        snapshot: &Snapshot,
        document: &Document,
        dependencies: &mut Dependencies,
    ) -> Result<u64> {
        let length: u64 = document.field("length")?;
        anyhow::ensure!(
            length <= MAX_MEDIA_BYTES && !document.inputs.is_empty(),
            "invalid media size"
        );
        let mut checked_policies = BTreeSet::new();
        for link in &document.inputs {
            let id = link.target;
            anyhow::ensure!(
                link.role == "chunk" && id.format().is_none(),
                "invalid media source"
            );
            if let Some(missing) = self.search_missing(snapshot, id)? {
                dependencies.missing.insert(missing);
                anyhow::bail!("media source is unavailable");
            }
            anyhow::ensure!(
                snapshot.objects.contains(&id),
                "media source is outside the snapshot"
            );
            let access: String = self.db.query_row(
                "SELECT access FROM objects WHERE id=? AND data IS NOT NULL",
                [id.to_string()],
                |row| row.get(0),
            )?;
            let access = access.parse()?;
            anyhow::ensure!(
                snapshot.objects.contains(&access),
                "missing media access descriptor"
            );
            if checked_policies.insert(access) {
                self.check_access(access, |policy| allows(policy, "read"))?;
            }
            dependencies.available.extend([id, access]);
        }
        Ok(length)
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
        document: &Document,
        location: &SearchLocation,
        resolve: impl FnMut(Id) -> Result<Vec<u8>>,
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
                let bytes =
                    VerifiedTextChunk::default().read_span(document, *start..*end, resolve)?;
                Some(String::from_utf8(bytes)?)
            }
        })
    }
}
