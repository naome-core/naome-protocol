//! Incremental read exchange. The last acknowledged member determines the shared tree path.
//! Receiving a page commits its verified packets and prefix counter in the same transaction.
use super::{Archive, Manifest, Packet, Pages, Witnesses, add_packet, member};
use crate::{
    store::{
        custody::tree::Tree,
        snapshot::{StoredHeader, save_imported_head},
    },
    wire::{Id, MAX_BYTES},
};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::{BufReader, Read},
};

const PAGE_LIMIT: usize = 1024;
const WIRE_BYTES: usize = 72 * 1024 * 1024;
const FORMAT: &str = "naome-inventory-page-1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangePage {
    format: String,
    manifest: Manifest,
    head: Id,
    after: Option<Id>,
    examined: usize,
    complete: bool,
    pages: Pages,
    packets: Vec<Packet>,
    last: Option<Id>,
}

#[derive(Serialize)]
pub struct ExchangeProgress {
    pub head: Id,
    pub continuation: Option<Id>,
    pub complete: bool,
    pub packets: usize,
}

impl ExchangePage {
    pub fn read(input: impl Read) -> Result<Self> {
        let mut input = input.take(WIRE_BYTES as u64 + 1);
        let page = serde_json::from_reader(BufReader::new(&mut input))?;
        ensure!(input.limit() > 0, "exchange page byte limit");
        Ok(page)
    }
    pub fn progress(&self) -> ExchangeProgress {
        ExchangeProgress {
            head: self.head,
            continuation: if self.complete { None } else { self.last },
            complete: self.complete,
            packets: self.packets.len(),
        }
    }
}

impl Archive {
    /// Export a consecutive range for reading. Pages on the preceding cursor's path are
    /// already present at a receiver that acknowledged that cursor at this immutable head.
    pub fn export_exchange_page(
        &self,
        head: Id,
        after: Option<Id>,
        limit: usize,
    ) -> Result<ExchangePage> {
        ensure!(
            (1..=PAGE_LIMIT).contains(&limit),
            "exchange page limit must be 1 through 1024"
        );
        self.with_read_view(|| {
            let m = self.active_manifest(head)?;
            let head = m.id()?;
            let mut tree = Tree::new(&self.db);
            let mut known = BTreeSet::new();
            if let Some(cursor) = after {
                let member = member::path(m.inventory, &cursor.to_string(), |id| {
                    known.insert(id);
                    tree.read(id)
                })?;
                ensure!(member == cursor, "cursor outside inventory");
            }
            let mut pages = Pages::new();
            let mut witness_bytes = 0;
            let mut packet_bytes = 0;
            let mut ids = Vec::new();
            let complete = m.scan_with(
                after,
                |id| {
                    let page = tree.read(id)?;
                    if !known.contains(&id) {
                        witness_bytes += serde_json::to_vec(&page)?.len() + 96;
                        ensure!(
                            witness_bytes <= 32 * 1024 * 1024,
                            "exchange range witness limit"
                        );
                        pages.insert(id, page.clone());
                    }
                    Ok(page)
                },
                |id| {
                    if ids.len() == limit {
                        return Ok(false);
                    }
                    let reserve = self.size(id)?.saturating_mul(2).saturating_add(512);
                    if !ids.is_empty() && packet_bytes + reserve > 2 * MAX_BYTES + 512 {
                        return Ok(false);
                    }
                    packet_bytes += reserve;
                    ids.push(id);
                    Ok(true)
                },
            )?;
            let mut access = Tree::new(&self.db);
            let mut used = witness_bytes + packet_bytes + 32768;
            let mut packets = Vec::new();
            for &id in &ids {
                if let Ok((bytes, policy)) =
                    self.resolve_inventory_member(&m, id, "read", &mut access)
                {
                    let packet = Packet {
                        id,
                        access: policy,
                        hex: hex::encode(bytes),
                    };
                    self.collect_linked_pages(&packet, &mut pages, &mut used, WIRE_BYTES)?;
                    packets.push(packet);
                }
            }
            Ok(ExchangePage {
                format: FORMAT.into(),
                manifest: m,
                head,
                after,
                last: ids.last().copied().or(after),
                examined: ids.len(),
                complete,
                pages,
                packets,
            })
        })
    }

    pub fn import_exchange_page(&mut self, page: &ExchangePage) -> Result<ExchangeProgress> {
        ensure!(
            page.format == FORMAT
                && page.examined <= PAGE_LIMIT
                && (page.examined != 0 || page.complete)
                && page.packets.len() <= page.examined,
            "invalid exchange range"
        );
        page.manifest.verify(page.head)?;
        let key = page.head.to_string();
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS archive_exchange_progress(
            head TEXT PRIMARY KEY, cursor TEXT, seen INTEGER NOT NULL, complete INTEGER NOT NULL
        ) WITHOUT ROWID;",
        )?;
        let (cursor, seen, complete): (Option<String>, i64, bool) = tx
            .query_row(
                "SELECT cursor,seen,complete FROM archive_exchange_progress WHERE head=?",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .unwrap_or_default();
        let cursor = cursor.map(|s| s.parse::<Id>()).transpose()?;
        let duplicate = complete || (cursor.is_some() && page.last <= cursor);
        ensure!(
            page.after == cursor || duplicate,
            "previous exchange page has not been acknowledged"
        );
        let witnesses = Witnesses::begin(&tx)?;
        for (&id, data) in &page.pages {
            witnesses.install(id, data)?;
        }
        let mut tree = Tree::new(&tx);
        let mut ids = Vec::new();
        let finished = page.manifest.scan_with(
            page.after,
            |id| {
                witnesses.used(id)?;
                tree.read(id)
            },
            |id| {
                if ids.len() == page.examined {
                    return Ok(false);
                }
                ids.push(id);
                Ok(true)
            },
        )?;
        ensure!(
            ids.len() == page.examined
                && finished == page.complete
                && ids.last().copied().or(page.after) == page.last,
            "exchange range differs from inventory"
        );
        let mut bytes = Vec::new();
        let mut previous = None;
        for p in &page.packets {
            ensure!(
                previous.is_none_or(|last| last < p.id) && ids.binary_search(&p.id).is_ok(),
                "packet outside exchange range"
            );
            add_packet(&tx, p, &mut bytes)?;
            witnesses.link(p.id, &bytes)?;
            previous = Some(p.id);
        }
        witnesses.finish()?;
        let mut progress = page.progress();
        if duplicate {
            progress.complete = complete;
            progress.continuation = if complete { None } else { cursor };
        } else {
            let total = u64::try_from(seen)?
                .checked_add(ids.len() as u64)
                .ok_or_else(|| anyhow::anyhow!("inventory count overflow"))?;
            ensure!(
                total <= page.manifest.count && (total == page.manifest.count) == finished,
                "exchange count differs from manifest"
            );
            if finished {
                let snapshot = StoredHeader::Current(page.manifest.clone()).into_snapshot();
                save_imported_head(&tx, page.head, &snapshot)?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO archive_exchange_progress VALUES(?,?,?,?)",
                params![
                    key,
                    page.last.map(|id| id.to_string()),
                    i64::try_from(total)?,
                    finished
                ],
            )?;
        }
        tx.commit()?;
        Ok(progress)
    }
}
