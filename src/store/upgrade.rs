//! The flat inventory decoder is confined to migration and immutable historical evidence.
use super::{Archive, Snapshot, inventory::bound_parents};
use crate::wire::Id;
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
#[cfg(feature = "cli")]
use std::path::Path;

const LEGACY_ANCESTRY_SOURCE: &str = "legacy snapshot ancestry";

impl Archive {
    /// Upgrade a consistent SQLite image at a new destination. Source and existing destinations are preserved.
    #[cfg(feature = "cli")]
    pub fn migrate_archive_file(source: &Path, destination: &Path) -> Result<usize> {
        use anyhow::Context;
        ensure!(source.is_file(), "source archive does not exist");
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let staged = tempfile::NamedTempFile::new_in(parent)?;
        let reader = Self::custody_reader(source)?;
        let archive_tables: i64 = reader.db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name IN ('objects','heads')",
            [],
            |row| row.get(0),
        )?;
        ensure!(archive_tables == 2, "source is not an archive");
        reader.db.execute(
            "VACUUM INTO ?",
            [staged.path().to_str().context("invalid output path")?],
        )?;
        drop(reader);
        let copy = Self::open(staged.path())?;
        let n: i64 = copy
            .db
            .query_row("SELECT (SELECT count(*) FROM archive_historical)+(SELECT count(*) FROM archive_prior_heads)", [], |r| r.get(0))?;
        drop(copy);
        staged.as_file().sync_all()?;
        staged.persist_noclobber(destination)?;
        Ok(usize::try_from(n)?)
    }
    pub(super) fn upgrade_catalog(&self) -> Result<()> {
        self.upgrade_exchange_staging()?;
        self.upgrade_full_seals()?;
        let old_exchange: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='archive_exchange_progress')",
            [],
            |row| row.get(0),
        )?;
        if old_exchange {
            self.db.execute_batch(
                "INSERT OR IGNORE INTO archive_exchange_sessions(head,verified,cursor,seen,complete)
                 SELECT head,0,cursor,seen,complete FROM archive_exchange_progress;
                 DROP TABLE archive_exchange_progress;",
            )?;
        }
        if self.db.execute(
            "INSERT OR IGNORE INTO custody_withdrawal_state VALUES(1)",
            [],
        )? != 0
        {
            self.db.execute("INSERT OR IGNORE INTO custody_withdrawal_log(object) SELECT id FROM objects WHERE data IS NULL ORDER BY id", [])?;
        }
        // The former WITHOUT ROWID table puts wide page payloads in an index b-tree.
        // Preserve each stored page (including physical splices) and its exact identifier.
        let old_pages: bool = self.db.query_row(
            "SELECT wr FROM pragma_table_list WHERE schema='main' AND name='custody_pages'",
            [],
            |r| r.get(0),
        )?;
        if old_pages {
            self.db.execute_batch(
                "ALTER TABLE custody_pages RENAME TO old_custody_pages;
                CREATE TABLE custody_pages(id TEXT PRIMARY KEY NOT NULL,data BLOB NOT NULL);
                INSERT INTO custody_pages SELECT id,data FROM old_custody_pages;
                DROP TABLE old_custody_pages;",
            )?;
        }
        let done: bool =
            self.db
                .query_row("SELECT EXISTS(SELECT 1 FROM archive_layout)", [], |r| {
                    r.get(0)
                })?;
        if done {
            // Draft migrations registered internal groups as tips without sealing them as snapshots.
            self.db.execute(
                "DELETE FROM custody_tips WHERE source=?
                 AND NOT EXISTS(SELECT 1 FROM heads WHERE id=custody_tips.head)",
                [LEGACY_ANCESTRY_SOURCE],
            )?;
            return Ok(());
        }
        let tx = &self.db;
        tx.execute(
            "INSERT OR IGNORE INTO archive_deposits(id) SELECT id FROM objects ORDER BY rowid",
            [],
        )?;
        // A payload may never have belonged to a stored cut.
        let mut large = self
            .db
            .prepare("SELECT id,data,access FROM objects WHERE length(data)>? ORDER BY rowid")?;
        let mut rows = large.query([super::custody::BLOCK as i64])?;
        while let Some(row) = rows.next()? {
            let id: Id = row.get::<_, String>(0)?.parse()?;
            if id.format().is_none() {
                super::extents::put(
                    &self.db,
                    id,
                    &row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?.parse()?,
                )?;
            }
        }
        drop(rows);
        drop(large);
        self.upgrade_prior_lineages()?;
        let upper: i64 =
            tx.query_row("SELECT coalesce(max(rowid),0) FROM heads", [], |r| r.get(0))?;
        let mut cursor = 0;
        loop {
            let old: Option<(i64, String, Vec<u8>)> = self
                .db
                .query_row(
                    "SELECT rowid,id,manifest FROM heads
                 WHERE rowid>? AND rowid<=? ORDER BY rowid LIMIT 1",
                    params![cursor, upper],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((row, id, bytes)) = old else { break };
            cursor = row;
            let previous: Id = id.parse()?;
            previous.verify(&bytes)?;
            let snapshot =
                serde_json::from_slice::<super::snapshot::StoredHeader>(&bytes)?.into_snapshot();
            if let Some(m) = &snapshot.manifest {
                m.verify(previous)?
            } else {
                self.convert_archived(previous, &snapshot)?;
            }
        }
        tx.execute("INSERT OR IGNORE INTO archive_layout VALUES(1)", [])?;
        Ok(())
    }

    /// Resolve historical identifiers for active operations while preserving exact historical replay through head().
    pub fn current_head_id(&self, id: Id) -> Result<Id> {
        let mapped: Option<String> = self
            .db
            .query_row(
                "SELECT current FROM archive_historical WHERE id=?1 UNION ALL SELECT current FROM archive_prior_heads WHERE id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        mapped.map_or(Ok(id), |s| s.parse())
    }
    pub(super) fn convert_archived(&self, old: Id, s: &Snapshot) -> Result<Id> {
        let tx = if self.db.is_autocommit() {
            Some(rusqlite::Transaction::new_unchecked(
                &self.db,
                TransactionBehavior::Immediate,
            )?)
        } else {
            None
        };
        ensure!(s.manifest.is_none(), "expected a historical flat inventory");
        s.verify_head(old)?;
        if let Some(id) = self
            .db
            .query_row(
                "SELECT current FROM archive_historical WHERE id=?",
                [old.to_string()],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            return id.parse();
        }
        let prior_row: i64 =
            self.db
                .query_row("SELECT coalesce(max(rowid),0) FROM heads", [], |r| r.get(0))?;
        let parents = bound_parents(s.parents.clone(), |group| {
            // Legacy parents can be external pins. Group them without loading their payloads
            // or adding synthetic inventories to the local search catalog.
            self.seal_ancestry_group(LEGACY_ANCESTRY_SOURCE, &group)
        })?;
        let next = self.seal_members(&s.source, parents, s.objects.clone())?;
        let current = next.id()?;
        let previous = old.to_string();
        let now = current.to_string();
        self.db.execute(
            "INSERT INTO archive_historical VALUES(?,?,?)",
            params![previous, s.bytes()?, now],
        )?;
        // Preserve disclosure, suspension and selection. The old bytes have their own evidence lookup.
        self.db.execute(
            "INSERT OR IGNORE INTO rep_imported_heads SELECT ? FROM rep_imported_heads
             WHERE head=? AND EXISTS(SELECT 1 FROM heads WHERE id=? AND rowid>?)",
            params![now, previous, now, prior_row],
        )?;
        self.migrate_release(old, current, &next)?;
        self.db.execute(
            "UPDATE rep_search_selection SET head=? WHERE head=?",
            params![now, previous],
        )?;
        for table in ["suspended_releases", "releases", "rep_imported_heads"] {
            self.db
                .execute(&format!("DELETE FROM {table} WHERE head=?"), [&previous])?;
        }
        self.db
            .execute("DELETE FROM heads WHERE id=?", [&previous])?;
        if let Some(tx) = tx {
            tx.commit()?
        }
        Ok(current)
    }
}

impl Archive {
    // The pre-release branch also circulated content-addressed delta heads. Their records
    // lived solely in segments. Reconstruct their forks before migrating scientific cuts.
    fn upgrade_prior_lineages(&self) -> Result<()> {
        let present: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='rep2_heads')",
            [],
            |r| r.get(0),
        )?;
        if !present {
            return Ok(());
        }
        #[derive(serde::Deserialize)]
        struct PriorHead {
            version: u8,
            source: String,
            parent: Option<Id>,
            delta: Vec<super::custody::Member>,
        }
        let mut q = self.db.prepare(
            "SELECT id,data FROM rep2_heads ORDER BY json_extract(data,'$.sequence'),id",
        )?;
        let mut rows = q.query([])?;
        while let Some(row) = rows.next()? {
            let original: Id = row.get::<_, String>(0)?.parse()?;
            let bytes: Vec<u8> = row.get(1)?;
            original.verify(&bytes)?;
            let h: PriorHead = serde_json::from_slice(&bytes)?;
            ensure!(h.version == 2, "unrecognized historical delta");
            let parent = h.parent.map(|p| self.current_head_id(p)).transpose()?;
            let mut items = Vec::new();
            for mut member in h.delta {
                let stored = if member.id.format().is_some() {
                    let (data, access): (Vec<u8>, String) = self.db.query_row(
                        "SELECT substr(p.data,r.start+1,r.length),r.access
                         FROM rep2_records r JOIN rep2_packs p ON p.id=r.pack WHERE r.id=?",
                        [member.id.to_string()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?;
                    Some((data, access.parse()?))
                } else {
                    self.raw(member.id)?
                };
                let data = if let Some((data, access)) = stored {
                    ensure!(access == member.access, "historical custody mismatch");
                    member.id.verify(&data)?;
                    data
                } else {
                    member.absent = true;
                    member.position = None;
                    Vec::new()
                };
                items.push(super::custody::Item {
                    member,
                    bytes: data,
                });
            }
            let admission = self.admit_batch(
                &h.source,
                parent,
                &items,
                &parent.into_iter().collect(),
                parent.is_none(),
            )?;
            let m = self.custody_head(admission.head)?.manifest()?;
            self.register_snapshot(&m, false)?;
            self.db.execute(
                "INSERT INTO archive_prior_heads VALUES(?,?,?)",
                params![original.to_string(), bytes, admission.head.to_string()],
            )?;
            ensure!(
                self.current_head_id(original)? == admission.head,
                "unverified historical head mapping"
            );
            let shared: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM rep2_shared WHERE head=?)",
                [original.to_string()],
                |r| r.get(0),
            )?;
            if shared && !self.custody_head(admission.head)?.restricted {
                self.db.execute(
                    "INSERT OR IGNORE INTO custody_shared VALUES(?)",
                    [admission.head.to_string()],
                )?;
            }
        }
        let mut receipts = self.db.prepare("SELECT id,data FROM rep2_receipts")?;
        let mut rows = receipts.query([])?;
        while let Some(row) = rows.next()? {
            let id: Id = row.get::<_, String>(0)?.parse()?;
            let data: Vec<u8> = row.get(1)?;
            id.verify(&data)?;
            self.db.execute(
                "INSERT OR IGNORE INTO custody_receipts VALUES(?,?)",
                params![id.to_string(), data],
            )?;
        }
        Ok(())
    }
}
