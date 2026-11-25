//! Candidate reverse links for native custody and verified read exchange.
use super::{Archive, InventoryReader};
use crate::wire::Id;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};

enum Source {
    Custody(Id),
    Exchange(Id),
}

pub(crate) struct BacklinkIndex {
    source: Source,
    pub(crate) complete: bool,
}

impl BacklinkIndex {
    /// The caller checks each candidate against the snapshot and its declared roles.
    pub(crate) fn candidates(
        &self,
        inventory: &mut InventoryReader<'_>,
        target: Id,
        limit: usize,
    ) -> Result<Vec<Id>> {
        match self.source {
            Source::Custody(root) => {
                let mut found = Vec::new();
                inventory
                    .tree
                    .prefix(root, &format!("r/{target}/"), limit, &mut found)?;
                Ok(found)
            }
            Source::Exchange(head) => inventory
                .tree
                .db
                .prepare_cached(
                    "SELECT record FROM archive_exchange_backlinks
                     WHERE head=? AND target=? ORDER BY record LIMIT ?",
                )?
                .query_map(
                    params![head.to_string(), target.to_string(), i64::try_from(limit)?],
                    |row| row.get::<_, String>(0),
                )?
                .map(|row| row?.parse())
                .collect(),
        }
    }
}

impl Archive {
    pub(crate) fn backlink_index(&self, head: Id) -> Result<Option<BacklinkIndex>> {
        if let Some(proof) = self.custody_head_if_present(head)? {
            return Ok(Some(BacklinkIndex {
                source: Source::Custody(proof.tree),
                complete: proof.opaque == 0,
            }));
        }
        let has_cache: bool = self.db.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='archive_exchange_backlinks')",
        )?.query_row([], |row| row.get(0))?;
        if !has_cache {
            return Ok(None);
        }
        // Pending sessions and membership-only exchanges cannot supply this index.
        let coverage: Option<bool> = self
            .db
            .prepare_cached(
                "SELECT coalesce(b.seen=s.seen AND b.covered,0) FROM archive_exchange_sessions s
             LEFT JOIN archive_exchange_backlink_state b ON b.head=s.head
             WHERE s.head=? AND s.verified AND s.complete AND (
                 b.head IS NOT NULL OR EXISTS(
                     SELECT 1 FROM archive_exchange_backlinks l WHERE l.head=s.head
                 )
             )",
            )?
            .query_row([head.to_string()], |row| row.get(0))
            .optional()?;
        Ok(coverage.map(|complete| BacklinkIndex {
            source: Source::Exchange(head),
            complete,
        }))
    }
}
