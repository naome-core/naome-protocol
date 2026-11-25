use super::{
    EVALUATOR_ID, Query, ReadInventory, Receipt, Standing, assess_with_inventory,
    check_receipt_budget, deposit_receipt_for_evaluator, manifest_encoding, receipt_document,
};
use crate::{
    migration::Equivalence,
    profile::{Document, Kind, PUBLIC},
    store::{Archive, Snapshot},
    wire::{Format, Id},
};
use anyhow::{Result, ensure};
use rusqlite::{Transaction, TransactionBehavior};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Serialize)]
pub struct ReceiptReplay {
    pub id: Id,
    pub head: Id,
    pub evaluator: String,
    pub matches: bool,
    pub recorded_standing: Option<Standing>,
    pub replayed: Receipt,
}

#[derive(Serialize)]
pub struct Reassessment {
    pub previous: Id,
    pub previous_head: Id,
    pub previous_standing: Option<Standing>,
    pub head: Id,
    pub id: Id,
    pub receipt: Receipt,
    pub new_obligations: BTreeSet<String>,
    pub cleared_obligations: BTreeSet<String>,
}

pub(crate) struct RebuiltReceipt {
    pub(crate) receipt: Receipt,
    identities: [Id; 2],
}

impl RebuiltReceipt {
    pub(crate) fn matches(&self, id: Id, aliases: &Equivalence) -> bool {
        self.identities
            .iter()
            .any(|&expected| aliases.equivalent(id, expected))
    }
}

pub(crate) fn rebuild_receipt(
    store: &Archive,
    historical: &Snapshot,
    pinned_head: Id,
    query: &Query,
    evaluator: &str,
    recorded: &Document,
    inventory: &mut ReadInventory,
) -> Result<RebuiltReceipt> {
    check_receipt_budget(query)?;
    ensure!(
        historical.id()? == pinned_head,
        "receipt horizon differs from the pinned head"
    );
    let receipt = assess_with_inventory(store, historical, query, evaluator, inventory)?.receipt;
    let predecessor = recorded
        .roles("reassessed-from")
        .next()
        .map(|_| recorded.one("reassessed-from"))
        .transpose()?;
    if let Some(predecessor) = predecessor {
        let prior = saved_assessment(store, historical, predecessor)?;
        ensure!(
            prior.field::<Query>("original_query")?.target == query.target,
            "reassessment predecessor covers another target"
        );
    }
    let expected = receipt_document(
        store,
        historical,
        pinned_head,
        query,
        &receipt,
        evaluator,
        predecessor,
    )?
    .encode(Id::blob(PUBLIC))?;
    let identities =
        [Format::A, Format::B].map(|format| Id::record(format, &expected.encode(format)));
    Ok(RebuiltReceipt {
        receipt,
        identities,
    })
}

fn saved_assessment(archive: &Archive, view: &Snapshot, receipt: Id) -> Result<Document> {
    assessment_document(archive.read(view, receipt, "read")?)
}

fn assessment_document(document: Document) -> Result<Document> {
    ensure!(
        document.kind == Kind::Assessment,
        "expected an assessment receipt"
    );
    Ok(document)
}

impl Archive {
    /// Re-run the recorded evaluator on the original evidence horizon without storing anything.
    pub fn replay_assessment(&self, view: &Snapshot, id: Id) -> Result<ReceiptReplay> {
        self.replay_receipt(Some(view), id)
    }

    /// Replay an authorized local receipt directly, including before the next snapshot.
    pub fn replay_local_assessment(&self, id: Id) -> Result<ReceiptReplay> {
        self.replay_receipt(None, id)
    }

    fn replay_receipt(&self, view: Option<&Snapshot>, id: Id) -> Result<ReceiptReplay> {
        self.with_read_view(|| {
            let recorded = match view {
                Some(view) => saved_assessment(self, view, id)?,
                None => assessment_document(self.read_with(id, |key| self.resolve_local(key))?)?,
            };
            let evaluator: String = recorded.field("evaluator")?;
            let query = recorded.field("original_query")?;
            let head = recorded.one("snapshot")?;
            let historical = match view {
                Some(view) => self.receipt_horizon(view, head)?,
                None => self.local_receipt_horizon(head)?,
            };
            let mut aliases = Equivalence::default();
            for mapping in recorded.roles("mapping") {
                aliases.add(self.verify_mapping_with(
                    view.unwrap_or(&historical),
                    mapping,
                    manifest_encoding(&evaluator)?,
                )?);
            }
            let replay = rebuild_receipt(
                self,
                &historical,
                head,
                &query,
                &evaluator,
                &recorded,
                &mut ReadInventory::for_replay(),
            )?;
            Ok(ReceiptReplay {
                id,
                head,
                evaluator,
                matches: replay.matches(id, &aliases),
                recorded_standing: recorded.field("result")?,
                replayed: replay.receipt,
            })
        })
    }

    /// Evaluate the saved query on an explicitly selected snapshot and retain the successor receipt.
    pub fn reassess(
        &self,
        head: Id,
        previous: Id,
        arguments: &[Id],
        current_at: Option<u64>,
        format: Format,
    ) -> Result<Reassessment> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let view = self.active_head(head)?;
        let recorded = saved_assessment(self, &view, previous)?;
        let original: Receipt = recorded.field("original_transcript")?;
        let mut query: Query = recorded.field("original_query")?;
        for &argument in arguments {
            if !query.arguments.contains(&argument) {
                query.arguments.push(argument);
            }
        }
        check_receipt_budget(&query)?;
        query.current_at = current_at.or(query.current_at);
        let receipt = super::assess_for_evaluator(self, &view, &query, EVALUATOR_ID)?;
        let id = deposit_receipt_for_evaluator(
            self,
            &view,
            &query,
            &receipt,
            format,
            EVALUATOR_ID,
            Some(previous),
        )?;
        let result = Reassessment {
            previous,
            previous_head: recorded.one("snapshot")?,
            previous_standing: original.result,
            head: view.id()?,
            id,
            new_obligations: receipt
                .obligations
                .difference(&original.obligations)
                .cloned()
                .collect(),
            cleared_obligations: original
                .obligations
                .difference(&receipt.obligations)
                .cloned()
                .collect(),
            receipt,
        };
        tx.commit()?;
        Ok(result)
    }
}
