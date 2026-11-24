use super::{Receipt, Standing, default_byte_limit};
use crate::{
    payload::CAPTURED_EVIDENCE_SCOPE,
    profile::{DependencyRole, Document, Kind, Link},
    store::{Archive, Snapshot, custody::BacklinkIndex},
    wire::Id,
};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Serialize)]
pub struct QuestionView {
    pub head: Id,
    pub question: Id,
    pub records: Vec<QuestionRecord>,
    pub unavailable_inputs: BTreeSet<Id>,
    pub limited_coverage: bool,
    pub unscanned_records_from: Option<Id>,
}

#[derive(Serialize)]
pub struct QuestionRecord {
    pub id: Id,
    pub kind: Kind,
    pub subject: String,
    pub scope: String,
    pub text: Option<String>,
    pub links: Vec<Link>,
    pub reported_standing: Option<Standing>,
    pub open_checks: BTreeSet<String>,
}

fn anchors(document: &Document, role: &str) -> bool {
    match document.kind {
        Kind::Plan => role == "question",
        Kind::Claim => matches!(role, "question" | "plan"),
        Kind::Occurrence | Kind::Derivation => {
            matches!(role, "question" | "plan" | "context" | "occurrence")
        }
        Kind::Argument => role == "claim",
        Kind::Assessment => matches!(role, "claim" | "argument"),
        Kind::Notice => matches!(role, "target" | "calibration" | "source-version"),
        Kind::Decision => matches!(role, "question" | "plan" | "claim" | "assessment"),
        _ => false,
    }
}

fn is_support(document: &Document, link: &Link) -> bool {
    matches!(
        document.dependency_role(link),
        DependencyRole::Evidence | DependencyRole::Outcome
    ) && !matches!(
        link.role.as_str(),
        "question"
            | "plan"
            | "context"
            | "policy"
            | "snapshot"
            | "profile"
            | "chunk"
            | "access"
            | "authority"
    )
}

enum QuestionInventory<'a> {
    Indexed {
        index: Box<BacklinkIndex<'a>>,
        candidates_left: usize,
        records_left: usize,
        bytes_left: usize,
    },
    Scanned {
        snapshot: &'a Snapshot,
        backlinks: BTreeMap<Id, Vec<Id>>,
    },
}

#[derive(PartialEq, Eq)]
enum RecordState {
    Readable,
    Unavailable,
    Unexamined,
}

struct QuestionWalk<'a> {
    archive: &'a Archive,
    inventory: QuestionInventory<'a>,
    documents: BTreeMap<Id, Document>,
    unavailable: BTreeSet<Id>,
    limited: bool,
    unscanned_from: Option<Id>,
}

impl QuestionWalk<'_> {
    fn load(&mut self, id: Id) -> Result<RecordState> {
        if self.documents.contains_key(&id) {
            return Ok(RecordState::Readable);
        }
        if self.unavailable.contains(&id) {
            return Ok(RecordState::Unavailable);
        }
        let (index, records_left, bytes_left) = match &mut self.inventory {
            QuestionInventory::Indexed {
                index,
                records_left,
                bytes_left,
                ..
            } => (index, records_left, bytes_left),
            QuestionInventory::Scanned { snapshot, .. } => {
                let unexamined = snapshot.objects.contains(&id)
                    && self.unscanned_from.is_some_and(|start| id >= start);
                return Ok(if unexamined {
                    RecordState::Unexamined
                } else {
                    RecordState::Unavailable
                });
            }
        };
        let size = self.archive.size(id)?;
        if *records_left == 0 || size > *bytes_left {
            self.limited = true;
            return Ok(RecordState::Unexamined);
        }
        *records_left -= 1;
        *bytes_left -= size;
        match index.read(id) {
            Ok(document) => {
                self.documents.insert(id, document);
                Ok(RecordState::Readable)
            }
            Err(_) => {
                self.unavailable.insert(id);
                self.limited = true;
                Ok(RecordState::Unavailable)
            }
        }
    }

    fn inbound(&mut self, target: Id, filter: impl Fn(&Document) -> bool) -> Result<Vec<Id>> {
        let candidates = match &mut self.inventory {
            QuestionInventory::Indexed {
                index,
                candidates_left,
                ..
            } => {
                let mut candidates = index.candidates(target, *candidates_left + 1)?;
                if candidates.len() > *candidates_left {
                    self.limited = true;
                    candidates.pop();
                }
                *candidates_left -= candidates.len();
                candidates
            }
            QuestionInventory::Scanned { backlinks, .. } => {
                backlinks.remove(&target).unwrap_or_default()
            }
        };
        let mut related = Vec::new();
        for id in candidates.into_iter().filter(|id| id.format().is_some()) {
            if self.load(id)? == RecordState::Readable {
                let document = &self.documents[&id];
                if filter(document)
                    && document
                        .inputs
                        .iter()
                        .any(|edge| edge.target == target && anchors(document, &edge.role))
                {
                    related.push(id);
                }
            }
        }
        Ok(related)
    }

    fn readable_blob(&mut self, id: Id) -> bool {
        match &mut self.inventory {
            QuestionInventory::Indexed { index, .. } => index.readable(id).is_ok(),
            QuestionInventory::Scanned { snapshot, .. } => {
                self.archive.readable(snapshot, id).is_ok()
            }
        }
    }

    fn walk(mut self, head: Id, question: Id) -> Result<QuestionView> {
        let mut related = BTreeSet::new();
        let mut queue = VecDeque::from([question]);
        queue.extend(
            self.documents[&question]
                .roles("claim")
                .filter(|id| id.format().is_some()),
        );
        while let Some(id) = queue.pop_front() {
            if self.load(id)? == RecordState::Readable && related.insert(id) {
                queue.extend(self.inbound(id, |_| true)?);
            }
        }

        let mut unavailable_inputs = BTreeSet::new();
        queue.extend(related.iter().copied());
        while let Some(id) = queue.pop_front() {
            let inputs: Vec<_> = self.documents[&id]
                .inputs
                .iter()
                .filter(|edge| is_support(&self.documents[&id], edge))
                .map(|edge| edge.target)
                .collect();
            for target in inputs {
                if target.format().is_none() {
                    if !self.readable_blob(target) {
                        unavailable_inputs.insert(target);
                    }
                    continue;
                }
                match self.load(target)? {
                    RecordState::Readable if related.insert(target) => {
                        queue.push_back(target);
                        for child in self.inbound(target, is_evidence_followup)? {
                            if related.insert(child) {
                                queue.push_back(child);
                            }
                        }
                    }
                    RecordState::Unavailable => {
                        unavailable_inputs.insert(target);
                    }
                    _ => {}
                }
            }
        }
        Ok(question_report(
            head,
            question,
            self.documents,
            related,
            unavailable_inputs,
            self.limited,
            self.unscanned_from,
        ))
    }
}

fn is_evidence_followup(document: &Document) -> bool {
    document.kind == Kind::Notice
        || (document.kind == Kind::Occurrence && document.scope == CAPTURED_EVIDENCE_SCOPE)
}

fn question_report(
    head: Id,
    question: Id,
    mut documents: BTreeMap<Id, Document>,
    related: BTreeSet<Id>,
    unavailable_inputs: BTreeSet<Id>,
    limited: bool,
    unscanned_records_from: Option<Id>,
) -> QuestionView {
    let records = related
        .iter()
        .map(|id| {
            let document = documents.remove(id).expect("related documents were read");
            let record: Option<Receipt> = (document.kind == Kind::Assessment)
                .then(|| document.field("original_transcript").ok())
                .flatten();
            let text = ["title", "statement", "note", "predicate", "reason"]
                .into_iter()
                .find_map(|key| document.field(key).ok());
            QuestionRecord {
                id: *id,
                kind: document.kind,
                subject: document.subject,
                scope: document.scope,
                text,
                links: document
                    .inputs
                    .into_iter()
                    .filter(|link| related.contains(&link.target))
                    .collect(),
                reported_standing: record.as_ref().and_then(|receipt| receipt.result),
                open_checks: record.map_or_else(BTreeSet::new, |receipt| receipt.obligations),
            }
        })
        .collect();
    QuestionView {
        head,
        question,
        records,
        limited_coverage: limited
            || unscanned_records_from.is_some()
            || !unavailable_inputs.is_empty(),
        unavailable_inputs,
        unscanned_records_from,
    }
}

impl Archive {
    /// Inspect a question in the explicitly selected or active search snapshot.
    pub fn question_at(
        &self,
        head: Option<Id>,
        question: Id,
        budget: usize,
    ) -> Result<QuestionView> {
        self.with_read_view(|| {
            let head = self.selected_search_id(head)?;
            self.inspect_question(head, question, budget, || {
                self.head(head).map(std::borrow::Cow::Owned)
            })
        })
    }

    /// Find the declared research around a question within a readable, bounded snapshot.
    pub fn question_view(
        &self,
        snapshot: &Snapshot,
        question: Id,
        budget: usize,
    ) -> Result<QuestionView> {
        self.with_read_view(|| {
            self.inspect_question(snapshot.id()?, question, budget, || {
                Ok(std::borrow::Cow::Borrowed(snapshot))
            })
        })
    }

    fn inspect_question<'a>(
        &self,
        head: Id,
        question: Id,
        budget: usize,
        fallback: impl FnOnce() -> Result<std::borrow::Cow<'a, Snapshot>>,
    ) -> Result<QuestionView> {
        ensure!(
            (1..=100_000).contains(&budget),
            "question scan budget must be between 1 and 100000"
        );
        if let Some(mut index) = self.backlink_index(head)? {
            let root = index.read(question)?;
            ensure!(root.kind == Kind::Question, "expected a question record");
            return QuestionWalk {
                archive: self,
                inventory: QuestionInventory::Indexed {
                    index: Box::new(index),
                    candidates_left: budget,
                    records_left: budget,
                    bytes_left: default_byte_limit(),
                },
                documents: BTreeMap::from([(question, root)]),
                unavailable: BTreeSet::new(),
                limited: false,
                unscanned_from: None,
            }
            .walk(head, question);
        }
        let snapshot = fallback()?;
        self.scan_question(&snapshot, head, question, budget)
    }

    fn scan_question(
        &self,
        snapshot: &Snapshot,
        head: Id,
        question: Id,
        budget: usize,
    ) -> Result<QuestionView> {
        snapshot.ensure_hydrated()?;
        let root = self.read(snapshot, question, "read")?;
        ensure!(root.kind == Kind::Question, "expected a question record");
        let mut documents = BTreeMap::from([(question, root)]);
        let mut remaining = default_byte_limit();
        let mut limited_coverage = false;
        let mut unscanned_records_from = None;
        for (index, id) in snapshot.records().filter(|id| *id != question).enumerate() {
            if index == budget {
                unscanned_records_from = Some(id);
                break;
            }
            let size = self.size(id)?;
            if size > remaining {
                unscanned_records_from = Some(id);
                break;
            }
            remaining -= size;
            match self.read(snapshot, id, "read") {
                Ok(document) => {
                    documents.insert(id, document);
                }
                Err(_) => limited_coverage = true,
            }
        }

        let mut backlinks: BTreeMap<Id, Vec<Id>> = BTreeMap::new();
        for (&id, document) in &documents {
            for edge in document
                .inputs
                .iter()
                .filter(|edge| anchors(document, &edge.role))
            {
                backlinks.entry(edge.target).or_default().push(id);
            }
        }
        QuestionWalk {
            archive: self,
            inventory: QuestionInventory::Scanned {
                snapshot,
                backlinks,
            },
            documents,
            unavailable: BTreeSet::new(),
            limited: limited_coverage,
            unscanned_from: unscanned_records_from,
        }
        .walk(head, question)
    }
}
