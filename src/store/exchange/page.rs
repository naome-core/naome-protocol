//! Incremental read exchange. The last acknowledged member determines the shared tree path.
//! Each page persists its verified packets and cursor. The final page admits the staged inventory.
use super::{
    Archive, Manifest, Packet, Pages, Witnesses,
    member::{committed_access, path},
};
use crate::{
    profile::{PUBLIC, READ_ONLY},
    store::{
        custody::{Head, MAX_INDEXED_EDGES, tree::Tree},
        insert,
        snapshot::{StoredHeader, save_imported_head},
    },
    wire::{Id, MAX_BYTES},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::{BufReader, Read},
};

const PAGE_LIMIT: usize = 1024;
const WIRE_BYTES: usize = 72 * 1024 * 1024;
const LEGACY_FORMAT: &str = "naome-inventory-page-1";
const FORMAT: &str = "naome-inventory-page-2";

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    descriptor: Option<Head>,
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
    /// Export a consecutive range with membership and custody access witnesses.
    pub fn export_exchange_page(
        &self,
        head: Id,
        after: Option<Id>,
        limit: usize,
    ) -> Result<ExchangePage> {
        self.make_exchange_page(head, after, limit, true)
    }

    /// Export the historical membership-only format for a recipient that trusts the sender.
    pub fn export_legacy_exchange_page(
        &self,
        head: Id,
        after: Option<Id>,
        limit: usize,
    ) -> Result<ExchangePage> {
        self.make_exchange_page(head, after, limit, false)
    }

    fn exchange_descriptor(&self, head: Id) -> Result<Head> {
        if let Some(descriptor) = self.custody_head_if_present(head)? {
            return Ok(descriptor);
        }
        let has_sessions: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='archive_exchange_sessions')",
            [],
            |row| row.get(0),
        )?;
        let bytes: Option<Vec<u8>> = if has_sessions {
            self.db
                .query_row(
                    "SELECT descriptor FROM archive_exchange_sessions
                 WHERE head=? AND verified AND complete",
                    [head.to_string()],
                    |row| row.get(0),
                )
                .optional()?
        } else {
            None
        };
        Ok(serde_json::from_slice(&bytes.context(
            "missing custody descriptor; use the original archive or a trusted legacy export",
        )?)?)
    }

    // The receiver already holds the inventory path for an acknowledged cursor.
    fn make_exchange_page(
        &self,
        head: Id,
        after: Option<Id>,
        limit: usize,
        verified: bool,
    ) -> Result<ExchangePage> {
        ensure!(
            (1..=PAGE_LIMIT).contains(&limit),
            "exchange page limit must be 1 through 1024"
        );
        self.with_read_view(|| {
            let m = self.active_manifest(head)?;
            let head = m.id()?;
            let descriptor = verified.then(|| self.exchange_descriptor(head)).transpose()?;
            if let Some(descriptor) = &descriptor {
                ensure!(descriptor.manifest()? == m, "custody descriptor differs from snapshot");
                ensure!(!descriptor.restricted, "cannot disclose custody witnesses from a restricted head; select public or read-only evidence, or use a trusted legacy export");
            }
            let mut tree = Tree::new(&self.db);
            let mut known = BTreeSet::new();
            if let Some(cursor) = after {
                let member = path(m.inventory, &cursor.to_string(), |id| {
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
                    if !known.contains(&id) && !pages.contains_key(&id) {
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
            let mut used = witness_bytes + packet_bytes + 32768
                + serde_json::to_vec(&descriptor)?.len();
            let mut packets = Vec::new();
            for &id in &ids {
                if let Ok((bytes, policy)) =
                    self.resolve_inventory_member(&m, id, "read", &mut access)
                {
                    if let Some(descriptor) = &descriptor {
                        verify_access(descriptor, id, policy, |key| {
                            if let Some(page) = pages.get(&key) {
                                return Ok(page.clone());
                            }
                            let page = tree.read(key)?;
                            used += serde_json::to_vec(&page)?.len() + 96;
                            ensure!(used <= WIRE_BYTES, "exchange custody witness limit");
                            pages.insert(key, page.clone());
                            Ok(page)
                        })?;
                    }
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
                format: if verified { FORMAT } else { LEGACY_FORMAT }.into(),
                manifest: m,
                head,
                after,
                last: ids.last().copied().or(after),
                examined: ids.len(),
                complete,
                pages,
                packets,
                descriptor,
            })
        })
    }

    pub fn import_exchange_page(&mut self, page: &ExchangePage) -> Result<ExchangeProgress> {
        self.import_pinned_exchange_page(page.head, page)
    }

    /// Verify the page at a head the caller obtained independently of the transfer.
    pub fn import_pinned_exchange_page(
        &mut self,
        head: Id,
        page: &ExchangePage,
    ) -> Result<ExchangeProgress> {
        self.admit_exchange_page(head, page, true)
    }

    /// Accept a historical page from a trusted sender. It carries no custody access witness.
    pub fn import_legacy_exchange_page(
        &mut self,
        head: Id,
        page: &ExchangePage,
    ) -> Result<ExchangeProgress> {
        self.admit_exchange_page(head, page, false)
    }

    fn admit_exchange_page(
        &mut self,
        head: Id,
        page: &ExchangePage,
        verified: bool,
    ) -> Result<ExchangeProgress> {
        ensure!(
            page.head == head,
            "exchange page does not match expected head"
        );
        let descriptor = match (verified, page.format.as_str(), &page.descriptor) {
            (true, FORMAT, Some(descriptor)) => {
                ensure!(
                    !descriptor.restricted && descriptor.manifest()? == page.manifest,
                    "invalid exchange custody descriptor"
                );
                Some(descriptor)
            }
            (false, LEGACY_FORMAT, None) => None,
            (true, LEGACY_FORMAT, _) => anyhow::bail!(
                "legacy page lacks custody witnesses; use --legacy-page only for a trusted sender"
            ),
            _ => anyhow::bail!("unsupported exchange page format"),
        };
        ensure!(
            page.examined <= PAGE_LIMIT
                && (page.examined != 0 || page.complete)
                && page.packets.len() <= page.examined,
            "invalid exchange range"
        );
        page.manifest.verify(head)?;
        let key = head.to_string();
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (cursor, seen, complete): (Option<String>, i64, bool) = tx
            .query_row(
                "SELECT cursor,seen,complete FROM archive_exchange_sessions WHERE head=? AND verified=?",
                params![key, verified],
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
        let mut link_budget = MAX_INDEXED_EDGES;
        let mut indexed_records = 0;
        let mut backlinks = verified
            .then(|| {
                tx.prepare_cached("INSERT OR IGNORE INTO archive_exchange_backlinks VALUES(?,?,?)")
            })
            .transpose()?;
        for p in &page.packets {
            ensure!(
                previous.is_none_or(|last| last < p.id) && ids.binary_search(&p.id).is_ok(),
                "packet outside exchange range"
            );
            if let Some(descriptor) = descriptor {
                verify_access(descriptor, p.id, p.access, |id| {
                    witnesses.used(id)?;
                    tree.read(id)
                })?;
            }
            let record = p.decode(&mut bytes)?;
            if complete {
                insert(&tx, p.id, &bytes, p.access)?;
            } else {
                stage_packet(&tx, &key, verified, p, &bytes)?;
            }
            if let Some(record) = record
                && let Some(backlinks) = backlinks.as_mut()
                && let Some(remaining) = link_budget.checked_sub(record.edges.len())
            {
                link_budget = remaining;
                indexed_records += 1;
                let record_id = p.id.to_string();
                for edge in record.edges {
                    backlinks.execute(params![key, edge.target.to_string(), record_id])?;
                }
            }
            witnesses.link(p.id, &bytes)?;
            previous = Some(p.id);
        }
        drop(backlinks);
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
            if let Some(descriptor) = descriptor {
                let covered = descriptor.opaque == 0
                    && indexed_records == ids.iter().filter(|id| id.format().is_some()).count();
                // Omitted or unindexed records leave coverage open. The seen
                // checkpoint also detects ranges accepted by earlier versions.
                tx.execute(
                    "INSERT INTO archive_exchange_backlink_state VALUES(
                         ?1,?2,?3 AND (?4=0 OR EXISTS (
                             SELECT 1 FROM archive_exchange_backlink_state
                             WHERE head=?1 AND seen=?4 AND covered
                         ))
                     ) ON CONFLICT(head) DO UPDATE SET seen=excluded.seen,covered=excluded.covered",
                    params![key, i64::try_from(total)?, covered, seen],
                )?;
            }
            if finished {
                publish_packets(&tx, &key, verified)?;
                let snapshot = StoredHeader::Current(page.manifest.clone()).into_snapshot();
                save_imported_head(&tx, head, &snapshot)?;
            }
            tx.execute(
                "INSERT INTO archive_exchange_sessions VALUES(?,?,?,?,?,?)
                 ON CONFLICT(head,verified) DO UPDATE SET
                     cursor=excluded.cursor,seen=excluded.seen,complete=excluded.complete",
                params![
                    key,
                    verified,
                    page.last.map(|id| id.to_string()),
                    i64::try_from(total)?,
                    finished,
                    descriptor.map(serde_json::to_vec).transpose()?,
                ],
            )?;
        }
        tx.commit()?;
        Ok(progress)
    }
}

pub(super) fn verify_access(
    descriptor: &Head,
    id: Id,
    access: Id,
    mut load: impl FnMut(Id) -> Result<super::inventory::Page>,
) -> Result<()> {
    let public = Id::blob(PUBLIC);
    ensure!(
        [public, Id::blob(READ_ONLY)].contains(&access)
            && committed_access(descriptor, id, &mut load)? == access
            && committed_access(descriptor, access, load)? == public,
        "packet access differs from custody commitment"
    );
    Ok(())
}

fn stage_packet(
    db: &Connection,
    head: &str,
    verified: bool,
    packet: &Packet,
    bytes: &[u8],
) -> Result<()> {
    ensure!(
        db.prepare_cached(
            "INSERT INTO archive_exchange_packets VALUES(?,?,?,?,?)
         ON CONFLICT(head,verified,id) DO UPDATE SET access=excluded.access
         WHERE access=excluded.access AND data=excluded.data",
        )?
        .execute(params![
            head,
            verified,
            packet.id.to_string(),
            packet.access.to_string(),
            bytes
        ])? == 1,
        "conflicting packet in pending exchange"
    );
    Ok(())
}

fn publish_packets(db: &Connection, head: &str, verified: bool) -> Result<()> {
    let mut pending = db.prepare_cached(
        "SELECT id,access,data FROM archive_exchange_packets WHERE head=? AND verified=? ORDER BY id",
    )?;
    let mut rows = pending.query(params![head, verified])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let access: String = row.get(1)?;
        let data: Vec<u8> = row.get(2)?;
        insert(db, id.parse()?, &data, access.parse()?)?;
    }
    drop(rows);
    db.execute(
        "DELETE FROM archive_exchange_packets WHERE head=? AND verified=?",
        params![head, verified],
    )?;
    Ok(())
}
