use super::{Receipt, Standing, default_byte_limit};
use crate::{
    payload::CAPTURED_EVIDENCE_SCOPE,
    profile::{DependencyRole, Document, Kind, Link},
    store::{Archive, Snapshot},
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

impl Archive {
    /// Find the declared research around a question within a readable, bounded snapshot.
    pub fn question_view(
        &self,
        snapshot: &Snapshot,
        question: Id,
        budget: usize,
    ) -> Result<QuestionView> {
        ensure!(
            (1..=100_000).contains(&budget),
            "question scan budget must be between 1 and 100000"
        );
        self.with_read_view(|| {
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
            let mut related = BTreeSet::new();
            let mut queue = VecDeque::from([question]);
            queue.extend(
                documents[&question]
                    .roles("claim")
                    .filter(|id| documents.contains_key(id)),
            );
            while let Some(id) = queue.pop_front() {
                if related.insert(id)
                    && let Some(children) = backlinks.get(&id)
                {
                    queue.extend(children);
                }
            }

            // Follow shared evidence forward without importing the unrelated arguments that also cite it.
            let mut unavailable_inputs = BTreeSet::new();
            queue.extend(related.iter().copied());
            while let Some(id) = queue.pop_front() {
                let document = &documents[&id];
                for edge in document
                    .inputs
                    .iter()
                    .filter(|edge| is_support(document, edge))
                {
                    if documents.contains_key(&edge.target) {
                        if related.insert(edge.target) {
                            queue.push_back(edge.target);
                            if let Some(children) = backlinks.get(&edge.target) {
                                for &child in children.iter().filter(|&&child| {
                                    let document = &documents[&child];
                                    document.kind == Kind::Notice
                                        || (document.kind == Kind::Occurrence
                                            && document.scope == CAPTURED_EVIDENCE_SCOPE)
                                }) {
                                    if related.insert(child) {
                                        queue.push_back(child);
                                    }
                                }
                            }
                        }
                    } else if !snapshot.objects.contains(&edge.target)
                        || (edge.target.format().is_some()
                            && unscanned_records_from.is_none_or(|start| edge.target < start))
                        || (edge.target.format().is_none()
                            && self.readable(snapshot, edge.target).is_err())
                    {
                        unavailable_inputs.insert(edge.target);
                    }
                }
            }

            let mut records = Vec::with_capacity(related.len());
            for id in related.iter().copied() {
                let document = documents.remove(&id).expect("related documents were read");
                let record: Option<Receipt> = (document.kind == Kind::Assessment)
                    .then(|| document.field("original_transcript").ok())
                    .flatten();
                let text = ["title", "statement", "note", "predicate", "reason"]
                    .into_iter()
                    .find_map(|key| document.field(key).ok());
                records.push(QuestionRecord {
                    id,
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
                });
            }

            Ok(QuestionView {
                head: snapshot.id()?,
                question,
                records,
                limited_coverage: limited_coverage
                    || unscanned_records_from.is_some()
                    || !unavailable_inputs.is_empty(),
                unavailable_inputs,
                unscanned_records_from,
            })
        })
    }
}
