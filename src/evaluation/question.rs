use super::{Receipt, Standing, default_byte_limit};
use crate::{
    payload::CAPTURED_EVIDENCE_SCOPE,
    profile::{DependencyRole, Document, Kind, Link},
    store::{Archive, BacklinkIndex, InventoryReader, Snapshot},
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

enum QuestionBacklinks {
    Indexed {
        index: BacklinkIndex,
        candidates_left: usize,
    },
    Scanned(BTreeMap<Id, Vec<Id>>),
}

#[derive(PartialEq, Eq)]
enum RecordState {
    Readable,
    Unavailable,
    Unexamined,
}

struct QuestionWalk<'a> {
    archive: &'a Archive,
    inventory: InventoryReader<'a>,
    backlinks: QuestionBacklinks,
    documents: BTreeMap<Id, Document>,
    unavailable: BTreeSet<Id>,
    records_left: usize,
    bytes_left: usize,
    limited: bool,
    unscanned_from: Option<Id>,
}

impl QuestionWalk<'_> {
    fn examine(&mut self, id: Id) -> Result<RecordState> {
        if self.records_left == 0 {
            self.limited = true;
            return Ok(RecordState::Unexamined);
        }
        let size = self.archive.size(id)?;
        if size > self.bytes_left {
            self.limited = true;
            return Ok(RecordState::Unexamined);
        }
        self.records_left -= 1;
        self.bytes_left -= size;
        match self.inventory.read(id) {
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

    fn load(&mut self, id: Id) -> Result<RecordState> {
        if self.documents.contains_key(&id) {
            return Ok(RecordState::Readable);
        }
        if self.unavailable.contains(&id) {
            return Ok(RecordState::Unavailable);
        }
        match self.backlinks {
            QuestionBacklinks::Indexed { .. } => self.examine(id),
            QuestionBacklinks::Scanned(_) => {
                if self.unscanned_from.is_some_and(|start| id >= start)
                    && self.inventory.contains(id)?
                {
                    Ok(RecordState::Unexamined)
                } else {
                    Ok(RecordState::Unavailable)
                }
            }
        }
    }

    fn scan_remaining(&mut self) -> Result<()> {
        // Keep all verified candidates already visited and use only the remaining budget.
        let mut after = None;
        'scan: loop {
            let page = self.inventory.records_after(after, 32)?;
            if page.is_empty() {
                break;
            }
            after = page.last().copied();
            for id in page {
                if self.documents.contains_key(&id) || self.unavailable.contains(&id) {
                    continue;
                }
                if self.examine(id)? == RecordState::Unexamined {
                    self.unscanned_from = Some(id);
                    break 'scan;
                }
            }
        }
        let mut backlinks: BTreeMap<Id, Vec<Id>> = BTreeMap::new();
        for (&id, document) in &self.documents {
            for edge in document
                .inputs
                .iter()
                .filter(|edge| anchors(document, &edge.role))
            {
                backlinks.entry(edge.target).or_default().push(id);
            }
        }
        self.backlinks = QuestionBacklinks::Scanned(backlinks);
        self.limited = !self.unavailable.is_empty();
        Ok(())
    }

    fn inbound(&mut self, target: Id, filter: impl Fn(&Document) -> bool) -> Result<Vec<Id>> {
        let candidates = match &mut self.backlinks {
            QuestionBacklinks::Indexed {
                index,
                candidates_left,
            } => {
                let mut candidates =
                    index.candidates(&mut self.inventory, target, *candidates_left + 1)?;
                if candidates.len() > *candidates_left {
                    self.limited = true;
                    candidates.pop();
                }
                *candidates_left -= candidates.len();
                candidates
            }
            QuestionBacklinks::Scanned(backlinks) => backlinks.remove(&target).unwrap_or_default(),
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

    fn related(&mut self, question: Id) -> Result<(BTreeSet<Id>, BTreeSet<Id>)> {
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
                    if self.inventory.readable(target).is_err() {
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
        Ok((related, unavailable_inputs))
    }

    fn report(mut self, head: Id, question: Id) -> Result<QuestionView> {
        match &self.backlinks {
            QuestionBacklinks::Indexed { index, .. } if index.complete => {}
            QuestionBacklinks::Indexed { .. } => {
                self.related(question)?;
                self.scan_remaining()?;
            }
            QuestionBacklinks::Scanned(_) => self.scan_remaining()?,
        }
        let (related, unavailable_inputs) = self.related(question)?;
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
            let inventory = self.inventory_reader(self.selected_search_id(head)?)?;
            self.inspect_question(inventory, question, budget)
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
            snapshot.ensure_hydrated()?;
            self.inspect_question(
                InventoryReader::materialized(self, snapshot),
                question,
                budget,
            )
        })
    }

    fn inspect_question(
        &self,
        mut inventory: InventoryReader<'_>,
        question: Id,
        budget: usize,
    ) -> Result<QuestionView> {
        ensure!(
            (1..=100_000).contains(&budget),
            "question scan budget must be between 1 and 100000"
        );
        let head = inventory.id()?;
        let root = inventory.read(question)?;
        ensure!(root.kind == Kind::Question, "expected a question record");
        let backlinks = match self.backlink_index(head)? {
            Some(index) => QuestionBacklinks::Indexed {
                index,
                candidates_left: budget,
            },
            None => QuestionBacklinks::Scanned(BTreeMap::new()),
        };
        QuestionWalk {
            archive: self,
            inventory,
            backlinks,
            documents: BTreeMap::from([(question, root)]),
            unavailable: BTreeSet::new(),
            records_left: budget,
            bytes_left: default_byte_limit(),
            limited: false,
            unscanned_from: None,
        }
        .report(head, question)
    }
}
