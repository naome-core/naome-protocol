//! Pull bounded deltas between independent custody databases. Each verified commit is restartable.
use super::*;
#[derive(Serialize)]
pub struct Pull {
    pub sender_io: PageIo,
    pub receiver_io: PageIo,
    pub target: Id,
    pub cursor: Id,
    pub complete: bool,
    pub batches: usize,
    pub payload_bytes: usize,
    pub descriptor_bytes: usize,
}

impl Archive {
    /// Private local branch tips for crash recovery. Historical parents stay addressable.
    pub fn custody_tips(&self, source: &str, after: Option<Id>, limit: usize) -> Result<Vec<Id>> {
        ensure!((1..=1024).contains(&limit), "invalid tip page size");
        let mut q = self.db.prepare(
            "SELECT head FROM custody_tips WHERE source=? AND head>? ORDER BY head LIMIT ?",
        )?;
        let rows = q.query_map(
            params![
                source,
                after.map(|v| v.to_string()).unwrap_or_default(),
                limit as i64
            ],
            |r| r.get::<_, String>(0),
        )?;
        rows.map(|r| r?.parse()).collect()
    }
    /// Explicitly share or retract this exact tip. Grants do not follow future forks.
    pub fn custody_share(&self, id: Id, visible: bool) -> Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        if visible {
            let h = self.custody_head(id)?;
            ensure!(!h.restricted, "restricted inventory");
            self.check_withdrawals(&h, &mut Tree::new(&self.db), usize::MAX)?;
            tx.execute("INSERT OR REPLACE INTO custody_grant_checks SELECT ?,coalesce(max(serial),0) FROM custody_withdrawal_log",[id.to_string()])?;
            tx.execute(
                "INSERT OR IGNORE INTO custody_shared VALUES(?)",
                [id.to_string()],
            )?;
        } else {
            tx.execute(
                "DELETE FROM custody_grant_checks WHERE head=?",
                [id.to_string()],
            )?;
            tx.execute("DELETE FROM custody_shared WHERE head=?", [id.to_string()])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub(super) fn permitted_pull(&self, target: Id, tip: &Head) -> Result<PageIo> {
        let shared: bool = self
            .db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM custody_shared WHERE head=?)")?
            .query_row([target.to_string()], |r| r.get(0))?;
        ensure!(
            shared && !tip.restricted,
            "head has no current replica grant"
        );
        let mut tree = Tree::new(&self.db);
        self.check_withdrawals(tip, &mut tree, 1024)?;
        Ok(tree.io)
    }
    fn jump(&self, id: Id, level: u32) -> Result<Option<Id>> {
        let value: Option<String> = self
            .db
            .prepare_cached("SELECT ancestor FROM custody_jumps WHERE head=? AND level=?")?
            .query_row(params![id.to_string(), level], |r| r.get(0))
            .optional()?;
        value.map(|s| s.parse()).transpose()
    }
    pub(super) fn custody_jumps(&self, mut ancestor: Option<Id>) -> Result<Vec<Id>> {
        let mut jumps = Vec::new();
        let mut level = 0;
        while let Some(a) = ancestor {
            jumps.push(a);
            ancestor = self.jump(a, level)?;
            level += 1;
        }
        Ok(jumps)
    }
    pub(super) fn ancestor(&self, mut head: Id, distance: u64) -> Result<Id> {
        let mut remaining = distance;
        while remaining != 0 {
            let level = remaining.trailing_zeros();
            head = self.jump(head, level)?.context("missing ancestry proof")?;
            remaining &= remaining - 1;
        }
        Ok(head)
    }
    fn admitted(&self, id: Id) -> Result<bool> {
        Ok(self
            .db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM custody_heads WHERE id=?)")?
            .query_row([id.to_string()], |r| r.get(0))?)
    }
    /// An operator supplies an independently pinned origin head. No socket or discovery authority is implied.
    pub fn custody_pull(&self, origin: &Archive, target: Id, budget: usize) -> Result<Pull> {
        ensure!((1..=256).contains(&budget), "invalid batch budget");
        self.pull_to(origin, target, budget as u64)
    }

    /// Follow one immutable target to completion. Each step independently checks its grant
    /// and commits its cursor, so interruption can resume with either pull entry point.
    pub fn custody_pull_until_complete(&self, origin: &Archive, target: Id) -> Result<Pull> {
        self.pull_to(origin, target, u64::MAX)
    }

    fn pull_to(&self, origin: &Archive, target: Id, budget: u64) -> Result<Pull> {
        let tip = origin.custody_head(target)?;
        ensure!(
            !tip.restricted,
            "whole-head transfer requires public read custody"
        );
        let mut sender_io = origin.with_read_view(|| origin.permitted_pull(target, &tip))?;
        let mut receiver_io = PageIo::default();
        let mut cursor = target;
        let mut seq = tip.sequence;
        if !self.admitted(target)? {
            let saved: Option<String> = self
                .db
                .query_row(
                    "SELECT cursor FROM custody_pull_cursors WHERE target=?",
                    [target.to_string()],
                    |r| r.get(0),
                )
                .optional()?;
            let prefix = saved
                .and_then(|s| s.parse::<Id>().ok())
                .and_then(|id| self.custody_head(id).ok().map(|h| (id, h.sequence)))
                .filter(|&(id, seq)| {
                    seq <= tip.sequence
                        && origin.ancestor(target, tip.sequence - seq).ok() == Some(id)
                });
            if let Some((id, verified)) = prefix
                && verified < tip.sequence
                && !self.admitted(origin.ancestor(target, tip.sequence - verified - 1)?)?
            {
                cursor = id;
                seq = verified;
            } else {
                // Find the longest verified prefix, including one previously received for another fork.
                let (mut low, mut high) = (0, tip.sequence);
                while low < high {
                    let mid = low + (high - low).div_ceil(2);
                    if self.admitted(origin.ancestor(target, tip.sequence - mid)?)? {
                        low = mid;
                    } else {
                        high = mid - 1;
                    }
                }
                seq = low;
                if seq > 0 {
                    cursor = origin.ancestor(target, tip.sequence - seq)?;
                }
            }
        }
        let mut cache = super::frame::ProofCache::default();
        let mut batches = 0;
        let mut payload_bytes = 0;
        let mut descriptor_bytes = 0;
        while seq < tip.sequence && (batches as u64) < budget {
            let after = (seq != 0).then_some(cursor);
            let (receipt, payload, descriptors, io) =
                self.direct_step(origin, target, &tip, after, seq + 1, &mut cache)?;
            sender_io += io;
            payload_bytes += payload;
            descriptor_bytes += descriptors;
            let next = receipt.head;
            receiver_io += receipt.io;
            seq += 1;
            batches += 1;
            cursor = next;
        }
        Ok(Pull {
            sender_io,
            receiver_io,
            target,
            cursor,
            complete: cursor == target,
            batches,
            payload_bytes,
            descriptor_bytes,
        })
    }
}
