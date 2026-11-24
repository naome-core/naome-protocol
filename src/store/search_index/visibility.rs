//! A fully prepared append parent can share its visible entries with a child.
//! Each sealed set is immutable except for global source withdrawals.
use super::{Archive, Snapshot, cache::coverage};
use anyhow::Result;
use rusqlite::{TransactionBehavior, params};

impl Archive {
    pub(super) fn initialize_visibility(&self, snapshot: &Snapshot, head: &str) -> Result<()> {
        if self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM rep_search_heads WHERE head=?)",
            [head],
            |r| r.get::<_, bool>(0),
        )? {
            return Ok(());
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        if tx.execute(
            "INSERT OR IGNORE INTO rep_search_heads(head) VALUES(?)",
            [head],
        )? != 0
            && let Ok(child) = self.custody_head(snapshot.id()?)
            && let Some(parent) = child.parent
        {
            // A verified append preserves old members. A partial or limited search view
            // cannot be inherited; missing evidence may have arrived in the child.
            let key = parent.to_string();
            let available: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM rep_search_heads WHERE head=?1 AND finished)
                AND NOT EXISTS(SELECT 1 FROM rep_search_waiting WHERE head=?1)
                AND (SELECT count(*) FROM rep_search_head_sets WHERE head=?1)<64",
                [&key],
                |r| r.get(0),
            )?;
            if available && coverage(&tx, &key)? == (true, false) {
                // Seal the parent's private set in place. Older heads and forks retain it
                // even if the original search view is explicitly cleared and rebuilt.
                tx.execute(
                    "UPDATE rep_search_sets SET writer=NULL WHERE writer=?",
                    [&key],
                )?;
                tx.execute("INSERT INTO rep_search_head_sets SELECT ?,set_id FROM rep_search_head_sets WHERE head=?",params![head,key])?;
                for member in child.delta {
                    if member.id.format().is_some() {
                        let id = member.id.to_string();
                        tx.execute("INSERT OR IGNORE INTO rep_search_waiting(source,head,object,ready) SELECT ?1,?2,?1,1
                            WHERE NOT EXISTS(SELECT 1 FROM rep_search_visible WHERE head=?2 AND object=?1)",params![id,head])?;
                    }
                }
                tx.execute(
                    "UPDATE rep_search_heads SET finished=1 WHERE head=?",
                    [head],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}
