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
        extents, insert,
        snapshot::{StoredHeader, save_imported_head},
        validate,
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

/// Durable checkpoint of a paged import for one verification mode.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ExchangeImportStatus {
    /// Whether the page format authenticated each supplied object's committed access.
    pub verified: bool,
    /// The last acknowledged inventory member, retained after completion.
    pub cursor: Option<Id>,
    pub examined: u64,
    /// Distinct packets retained at this checkpoint. Completion freezes the admitted count.
    pub supplied: Option<u64>,
    /// Examined members with no packet supplied at this checkpoint, when recorded.
    pub omitted: Option<u64>,
    /// Whether the entire inventory range has been acknowledged.
    pub complete: bool,
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
    /// Inspect paged imports at this exact head without changing the archive.
    pub fn exchange_import_status(&self, head: Id) -> Result<Vec<ExchangeImportStatus>> {
        self.with_read_view(|| {
            let sql = if has_table(&self.db, "archive_exchange_sessions")? {
                if has_supplied_column(&self.db)? {
                    "SELECT verified,cursor,seen,supplied,complete
                     FROM archive_exchange_sessions WHERE head=? ORDER BY verified DESC"
                } else if has_table(&self.db, "archive_exchange_packets")? {
                    "SELECT verified,cursor,seen,CASE WHEN verified AND NOT complete THEN (
                        SELECT count(*) FROM archive_exchange_packets p
                        WHERE p.head=s.head AND p.verified=s.verified
                     ) END,complete FROM archive_exchange_sessions s WHERE head=? ORDER BY verified DESC"
                } else {
                    "SELECT verified,cursor,seen,NULL,complete
                     FROM archive_exchange_sessions WHERE head=? ORDER BY verified DESC"
                }
            } else if has_table(&self.db, "archive_exchange_progress")? {
                "SELECT 0,cursor,seen,NULL,complete FROM archive_exchange_progress WHERE head=?"
            } else {
                return Ok(Vec::new());
            };
            let mut query = self.db.prepare_cached(sql)?;
            let mut rows = query.query([head.to_string()])?;
            let mut sessions = Vec::new();
            while let Some(row) = rows.next()? {
                let examined = u64::try_from(row.get::<_, i64>(2)?)?;
                let supplied = row.get::<_, Option<i64>>(3)?.map(u64::try_from).transpose()?;
                let omitted = supplied
                    .map(|n| examined.checked_sub(n).context("invalid exchange packet count"))
                    .transpose()?;
                sessions.push(ExchangeImportStatus {
                    verified: row.get(0)?,
                    cursor: row.get::<_, Option<String>>(1)?.map(|id| id.parse()).transpose()?,
                    examined,
                    supplied,
                    omitted,
                    complete: row.get(4)?,
                });
            }
            Ok(sessions)
        })
    }

    /// Upgrade staging inside archive initialization, preserving pending and completed cursors.
    pub(in crate::store) fn upgrade_exchange_staging(&self) -> Result<()> {
        let db = &self.db;
        if !has_supplied_column(db)? {
            db.execute_batch(
                "ALTER TABLE archive_exchange_sessions ADD COLUMN
                 supplied INTEGER CHECK(supplied BETWEEN 0 AND seen);",
            )?;
        }
        if has_table(db, "archive_exchange_packets")? {
            let mut query =
                db.prepare("SELECT head,verified,id,access,data FROM archive_exchange_packets")?;
            let mut rows = query.query([])?;
            while let Some(row) = rows.next()? {
                let head: String = row.get(0)?;
                let verified: bool = row.get(1)?;
                let id: Id = row.get::<_, String>(2)?.parse()?;
                let access: Id = row.get::<_, String>(3)?.parse()?;
                let bytes: Vec<u8> = row.get(4)?;
                validate(id, &bytes, access)?;
                stage_packet(db, &head, verified, id, access, &bytes)?;
            }
            drop(rows);
            drop(query);
            db.execute_batch(
                "UPDATE archive_exchange_sessions AS s SET supplied=(
                    SELECT count(*) FROM archive_exchange_staged p
                    WHERE p.head=s.head AND p.verified=s.verified
                 ) WHERE s.verified AND NOT s.complete;
                 DROP TABLE archive_exchange_packets;",
            )?;
        }
        Ok(())
    }

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

    fn imported_exchange_descriptor(&self, head: Id) -> Result<Option<Head>> {
        if !has_table(&self.db, "archive_exchange_sessions")? {
            return Ok(None);
        }
        let bytes: Option<Vec<u8>> = self
            .db
            .prepare_cached(
                "SELECT descriptor FROM archive_exchange_sessions
             WHERE head=? AND verified AND complete",
            )?
            .query_row([head.to_string()], |row| row.get(0))
            .optional()?;
        let descriptor: Option<Head> = bytes.as_deref().map(serde_json::from_slice).transpose()?;
        if let Some(descriptor) = &descriptor {
            ensure!(
                !descriptor.restricted && descriptor.manifest()?.id()? == head,
                "invalid imported custody descriptor"
            );
        }
        Ok(descriptor)
    }

    fn exchange_descriptor(&self, head: Id) -> Result<Head> {
        if let Some(descriptor) = self.custody_head_if_present(head)? {
            return Ok(descriptor);
        }
        self.imported_exchange_descriptor(head)?.context(
            "missing custody descriptor; use the original archive or a trusted legacy export",
        )
    }

    pub(in crate::store) fn exchange_access_scope(&self, head: Id) -> Result<Option<Id>> {
        let scoped = (has_table(&self.db, "archive_exchange_sessions")?
            && self.db.prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM archive_exchange_sessions WHERE head=? AND verified AND complete)",
            )?.query_row([head.to_string()], |row| row.get::<_, bool>(0))?)
            || (has_table(&self.db, "archive_member_access")?
                && self.db.prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM archive_member_access WHERE head=?)",
                )?.query_row([head.to_string()], |row| row.get::<_, bool>(0))?);
        Ok(scoped.then_some(head))
    }

    // Search an authenticated custody path even if the particular member was omitted.
    // The receiver's global object catalog cannot substitute for that proof.
    pub(in crate::store) fn require_exchange_access(
        &self,
        head: Id,
        member: Id,
        access: Id,
        point_read: bool,
    ) -> Result<()> {
        if let Some(descriptor) = self.imported_exchange_descriptor(head)? {
            let mut tree = Tree::new(&self.db);
            let proof = verify_access(&descriptor, member, access, |id| tree.read(id));
            if proof.is_err() && point_read && self.point_access(head, member)? == Some(access) {
                return Ok(());
            }
            proof.context("unavailable authenticated exchange access")?;
        } else if let Some(agreed) =
            self.stored_member_access("archive_member_access", head, member)?
        {
            ensure!(
                access == agreed
                    || (point_read && self.point_access(head, member)? == Some(access)),
                "local ACL differs from scoped exchange access"
            );
        }
        Ok(())
    }

    pub(in crate::store) fn point_access(&self, head: Id, member: Id) -> Result<Option<Id>> {
        if let Some(access) = self.stored_member_access("archive_point_access", head, member)? {
            return Ok(Some(access));
        }
        // Older point proofs share a table with legacy conflicts. A conflict
        // row can never equal the immutable ACL of the locally held object.
        self.stored_member_access("archive_member_access", head, member)
    }

    fn stored_member_access(&self, table: &str, head: Id, member: Id) -> Result<Option<Id>> {
        if !has_table(&self.db, table)? {
            return Ok(None);
        }
        let agreed: Option<String> = self
            .db
            .prepare_cached(&format!(
                "SELECT access FROM {table} WHERE head=? AND member=?"
            ))?
            .query_row(params![head.to_string(), member.to_string()], |row| {
                row.get(0)
            })
            .optional()?;
        agreed.map(|access| access.parse()).transpose()
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
                    self.resolve_inventory_member(&m, id, "read", false, &mut access)
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

    /// Accept a historical page from a trusted sender using its stated head.
    /// Use [Self::import_pinned_exchange_page] for the verified format.
    pub fn import_exchange_page(&mut self, page: &ExchangePage) -> Result<ExchangeProgress> {
        if page.format == FORMAT {
            anyhow::bail!("verified exchange requires an independently obtained head");
        }
        self.import_legacy_exchange_page(page.head, page)
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
        let (cursor, seen, complete, prior_supplied): (Option<String>, i64, bool, Option<i64>) = tx
            .query_row(
                "SELECT cursor,seen,complete,supplied FROM archive_exchange_sessions WHERE head=? AND verified=?",
                params![key, verified],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
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
        let mut supplied = 0;
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
            if conflicts_with_local_access(&tx, p.id, p.access)? {
                // A range acknowledged earlier can be redelivered after local admission
                // made its payload incompatible. Preserve the acknowledgement and omission.
                ensure!(
                    duplicate,
                    "conflicting access for immutable object {}",
                    p.id
                );
                witnesses.link(p.id, &bytes)?;
                previous = Some(p.id);
                continue;
            }
            if complete {
                insert(&tx, p.id, &bytes, p.access)?;
            } else {
                supplied += i64::from(stage_packet(&tx, &key, verified, p.id, p.access, &bytes)?);
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
            if supplied != 0 {
                tx.execute(
                    "UPDATE archive_exchange_sessions SET supplied=supplied+?
                     WHERE head=? AND verified=?",
                    params![supplied, key, verified],
                )?;
            }
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
                let conflicts = publish_packets(&tx, &key, verified)?;
                supplied -= conflicts;
                let snapshot = StoredHeader::Current(page.manifest.clone()).into_snapshot();
                save_imported_head(&tx, head, &snapshot)?;
                if verified || conflicts != 0 {
                    crate::store::search_index::invalidate_access_scope(&tx, &key)?;
                }
            }
            tx.execute(
                "INSERT INTO archive_exchange_sessions(head,verified,cursor,seen,complete,descriptor,supplied)
                 VALUES(?,?,?,?,?,?,?) ON CONFLICT(head,verified) DO UPDATE SET
                     cursor=excluded.cursor,seen=excluded.seen,complete=excluded.complete,
                     supplied=excluded.supplied",
                params![
                    key,
                    verified,
                    page.last.map(|id| id.to_string()),
                    i64::try_from(total)?,
                    finished,
                    descriptor.map(serde_json::to_vec).transpose()?,
                    if cursor.is_none() && seen == 0 { Some(supplied) }
                    else { prior_supplied.map(|n| n + supplied) },
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

fn has_table(db: &Connection, table: &str) -> Result<bool> {
    Ok(db
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?)")?
        .query_row([table], |row| row.get(0))?)
}

fn has_supplied_column(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('archive_exchange_sessions')
         WHERE name='supplied')",
        [],
        |row| row.get(0),
    )?)
}

fn stage_packet(
    db: &Connection,
    head: &str,
    verified: bool,
    id: Id,
    access: Id,
    bytes: &[u8],
) -> Result<bool> {
    let id = id.to_string();
    let access = access.to_string();
    let added = db
        .prepare_cached("INSERT OR IGNORE INTO archive_exchange_staged VALUES(?,?,?,?)")?
        .execute(params![head, verified, id, access])?
        != 0;
    if !added {
        let same: bool = db
            .prepare_cached(
                "SELECT access=? FROM archive_exchange_staged WHERE head=? AND verified=? AND id=?",
            )?
            .query_row(params![access, head, verified, id], |row| row.get(0))?;
        ensure!(same, "conflicting packet in pending exchange");
    } else {
        let admitted: bool = db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM objects WHERE id=?)")?
            .query_row([&id], |row| row.get(0))?;
        if !admitted {
            extents::store(db, &id, bytes)?;
        }
    }
    Ok(added)
}

fn conflicts_with_local_access(db: &Connection, id: Id, access: Id) -> Result<bool> {
    Ok(db
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM objects WHERE id=? AND access<>?)")?
        .query_row([id.to_string(), access.to_string()], |row| row.get(0))?)
}

fn publish_packets(db: &Connection, head: &str, verified: bool) -> Result<i64> {
    if !verified {
        // Legacy senders are trusted for their packet ACL. Preserve the agreed ACL for
        // a conflicting member even though the receiver cannot admit its payload.
        db.execute(
            "INSERT OR IGNORE INTO archive_member_access(head,member,access)
             SELECT p.head,p.id,p.access FROM archive_exchange_staged p JOIN objects o ON o.id=p.id
             WHERE p.head=? AND NOT p.verified AND o.access<>p.access",
            [head],
        )?;
    }
    // A local admission between pages owns its immutable ACL. Keep the remote member
    // in the pinned inventory, but treat the conflicting payload as omitted.
    let conflicts = db.execute(
        "DELETE FROM archive_exchange_staged AS p WHERE p.head=? AND p.verified=?
         AND EXISTS(SELECT 1 FROM objects o WHERE o.id=p.id AND o.access<>p.access)",
        params![head, verified],
    )?;
    // The content is already in its final storage. Catalog membership controls visibility.
    db.execute(
        "INSERT OR IGNORE INTO objects(id,data,access)
         SELECT id,x'',access FROM archive_exchange_staged WHERE head=? AND verified=? ORDER BY id",
        params![head, verified],
    )?;
    crate::store::point::promote_staged(db, head, verified)?;
    db.execute(
        "DELETE FROM archive_exchange_staged WHERE head=? AND verified=?",
        params![head, verified],
    )?;
    Ok(i64::try_from(conflicts)?)
}
