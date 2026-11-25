use super::{
    EVALUATOR_ID, Query, ReadInventory, Receipt, Standing, assess_with_inventory,
    check_receipt_budget, deposit_receipt_for_evaluator, manifest_encoding, receipt_document,
};
use crate::{
    migration::Equivalence,
    profile::{Document, Kind, PUBLIC},
    store::{Archive, InventoryReader, Snapshot},
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
    /// Re-run the recorded evaluator with its saved limits on the original evidence horizon.
    pub fn replay_assessment(&self, view: &Snapshot, id: Id) -> Result<ReceiptReplay> {
        self.replay_assessment_with_mappings(view, id, &[])
    }

    /// Replay a receipt using additional checked format mappings from the selected view.
    pub fn replay_assessment_with_mappings(
        &self,
        view: &Snapshot,
        id: Id,
        mappings: &[Id],
    ) -> Result<ReceiptReplay> {
        self.replay_receipt(
            Some(InventoryReader::materialized(self, view)),
            id,
            mappings,
        )
    }

    #[cfg(feature = "cli")]
    pub(crate) fn replay_assessment_at(
        &self,
        head: Id,
        id: Id,
        mappings: &[Id],
    ) -> Result<ReceiptReplay> {
        self.with_read_view(|| {
            let header = self.snapshot_header(self.selected_search_id(Some(head))?)?;
            let view = match &header.manifest {
                Some(manifest) => InventoryReader::paged(self, manifest.clone()),
                None => InventoryReader::materialized(self, &header),
            };
            self.replay_receipt(Some(view), id, mappings)
        })
    }

    /// Replay an authorized local receipt directly, including before the next snapshot.
    pub fn replay_local_assessment(&self, id: Id) -> Result<ReceiptReplay> {
        self.replay_receipt(None, id, &[])
    }

    fn replay_receipt(
        &self,
        mut view: Option<InventoryReader<'_>>,
        id: Id,
        mappings: &[Id],
    ) -> Result<ReceiptReplay> {
        self.with_read_view(|| {
            let recorded = match view.as_mut() {
                Some(view) => assessment_document(view.read(id)?)?,
                None => assessment_document(self.read_with(id, |key| self.resolve_local(key))?)?,
            };
            let evaluator: String = recorded.field("evaluator")?;
            let query = recorded.field("original_query")?;
            let head = recorded.one("snapshot")?;
            let historical = match view.as_mut() {
                Some(view) => self.receipt_horizon_in(view, head)?,
                None => self.local_receipt_horizon(head)?,
            };
            let mut aliases = Equivalence::default();
            let mut local = InventoryReader::materialized(self, &historical);
            let verification_mappings: BTreeSet<_> = recorded
                .roles("mapping")
                .chain(mappings.iter().copied())
                .collect();
            for mapping in verification_mappings {
                let encoding = manifest_encoding(&evaluator)?;
                aliases.add(match view.as_mut() {
                    Some(view) => self.verify_mapping_in(view, mapping, encoding)?,
                    None => self.verify_mapping_in(&mut local, mapping, encoding)?,
                });
            }
            let replay = rebuild_receipt(
                self,
                &historical,
                head,
                &query,
                &evaluator,
                &recorded,
                &mut ReadInventory::default(),
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
        self.reassess_with_mappings(head, previous, arguments, &[], current_at, format)
    }

    /// Reassess with additional native format mappings in the selected inventory.
    pub fn reassess_with_mappings(
        &self,
        head: Id,
        previous: Id,
        arguments: &[Id],
        mappings: &[Id],
        current_at: Option<u64>,
        format: Format,
    ) -> Result<Reassessment> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let view = self.active_head(head)?;
        let recorded = saved_assessment(self, &view, previous)?;
        let mut query: Query = recorded.field("original_query")?;
        check_receipt_budget(&query)?;
        let original: Receipt = recorded.field("original_transcript")?;
        for &argument in arguments {
            if !query.arguments.contains(&argument) {
                query.arguments.push(argument);
            }
        }
        for &mapping in mappings {
            if !query.mappings.contains(&mapping) {
                query.mappings.push(mapping);
            }
        }
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
