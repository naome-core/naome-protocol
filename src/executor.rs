use crate::{
    profile::{Kind, verify_attestation},
    store::{Archive, Snapshot},
    wire::Id,
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub attempt: String,
    pub actor: String,
    pub subject: String,
    pub operation: String,
    pub decision: Id,
    pub authority: Id,
    pub signature: Id,
    pub costs: BTreeMap<String, u64>,
}
#[derive(Debug, Serialize, PartialEq, Eq)]
pub enum Dispatch {
    Reserved,
    AlreadyRecorded {
        state: String,
        witness: Option<String>,
    },
}
impl Archive {
    /// An operator configures the shared pool once. A retry cannot replenish capacity.
    pub fn resource(&self, unit: &str, capacity: u64) -> Result<()> {
        ensure!(!unit.is_empty(), "empty unit");
        self.db.execute(
            "INSERT INTO limits VALUES(?,?)",
            params![unit, i64::try_from(capacity)?],
        )?;
        Ok(())
    }
    /// Commit this reservation before invoking the facility. A restart leaves an ambiguous reservation closed to redispatch.
    pub fn reserve(
        &mut self,
        s: &Snapshot,
        r: &Request,
        keys: &BTreeSet<String>,
        now: u64,
    ) -> Result<Dispatch> {
        ensure!(
            !r.attempt.is_empty() && !r.costs.is_empty(),
            "empty attempt or resource request"
        );
        let bytes = serde_json::to_vec(r)?;
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let old: Option<(Vec<u8>, String, Option<String>)> = tx
            .query_row(
                "SELECT request,state,witness FROM attempts WHERE id=?",
                [&r.attempt],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((old, state, witness)) = old {
            ensure!(
                old == bytes,
                "attempt identifier reused for a different request"
            );
            return Ok(Dispatch::AlreadyRecorded { state, witness });
        }
        let a = self.read_public(s, r.authority)?;
        ensure!(
            a.kind == Kind::Authority
                && a.subject == r.subject
                && a.field::<String>("actor")? == r.actor
                && a.field::<String>("operation")? == r.operation,
            "authority scope mismatch"
        );
        verify_attestation(
            &self.read_public(s, r.signature)?,
            r.authority,
            &a.scope,
            "executor",
            "research-policy-1",
            keys,
        )?;
        let [begin, end]: [u64; 2] = a.field("valid")?;
        ensure!(
            begin <= now && now <= end,
            "authority outside validity period"
        );
        let d = self.read_public(s, r.decision)?;
        ensure!(
            d.kind == Kind::Decision
                && d.subject == r.subject
                && d.scope == a.scope
                && d.one("authority")? == r.authority
                && d.field::<String>("attempt")? == r.attempt
                && d.field::<String>("operation")? == r.operation,
            "decision mismatch"
        );
        let max: BTreeMap<String, u64> = a.field("ceilings")?;
        let revoked: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM revocations WHERE authority=?)",
            [r.authority.to_string()],
            |row| row.get(0),
        )?;
        ensure!(!revoked, "authority revoked");
        initialize_attempt_spend(&tx)?;
        let mut spent = tx.prepare_cached(
            "SELECT spent FROM rep_attempt_spend WHERE authority=? AND resource=?",
        )?;
        let authority = r.authority.to_string();
        for (unit, &cost) in &r.costs {
            let prior: i64 = spent
                .query_row(params![authority, unit], |row| row.get(0))
                .optional()?
                .unwrap_or(0);
            let total = u64::try_from(prior)?
                .checked_add(cost)
                .context("quota overflow")?;
            ensure!(
                cost > 0 && max.get(unit).is_some_and(|cap| total <= *cap),
                "authority quota exceeded"
            );
            let cost = i64::try_from(cost)?;
            ensure!(
                tx.execute(
                    "UPDATE limits SET capacity=capacity-? WHERE resource=? AND capacity>=?",
                    params![cost, unit, cost]
                )? == 1,
                "shared resource unavailable"
            );
        }
        drop(spent);
        tx.execute(
            "INSERT INTO attempts VALUES(?,?,'execution-unknown',NULL)",
            params![r.attempt, bytes],
        )?;
        tx.commit()?;
        Ok(Dispatch::Reserved)
    }
    pub fn revoke(&self, authority: Id) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO revocations VALUES(?)",
            [authority.to_string()],
        )?;
        Ok(())
    }
    pub fn reconcile_attempt(
        &self,
        s: &Snapshot,
        attempt: &str,
        occurrence: Id,
        signature: Id,
        keys: &BTreeSet<String>,
    ) -> Result<()> {
        let input: Vec<u8> =
            self.db
                .query_row("SELECT request FROM attempts WHERE id=?", [attempt], |r| {
                    r.get(0)
                })?;
        let req: Request = serde_json::from_slice(&input)?;
        let d = self.read(s, occurrence, "read")?;
        ensure!(
            d.kind == Kind::Occurrence
                && d.subject == req.subject
                && d.field::<String>("attempt")? == attempt
                && d.one("decision")? == req.decision,
            "wrong readback"
        );
        verify_attestation(
            &self.read(s, signature, "read")?,
            occurrence,
            &d.scope,
            "readback",
            "research-policy-1",
            keys,
        )?;
        let state: String = d.field("execution")?;
        ensure!(
            ["observed", "not-executed"].contains(&state.as_str()),
            "invalid readback"
        );
        let updated = self.db.execute(
            "UPDATE attempts SET state=?,witness=? WHERE id=? AND (witness IS NULL OR witness=?)",
            params![
                state,
                occurrence.to_string(),
                attempt,
                occurrence.to_string()
            ],
        )?;
        if updated == 0 {
            bail!("conflicting readbacks require investigation")
        }
        Ok(())
    }
}

// Build the aggregate once from the authoritative attempts. SQLite maintains it
// for every new attempt, including writes made by an older REP binary.
fn initialize_attempt_spend(tx: &Transaction<'_>) -> Result<()> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='rep_attempt_spend')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        tx.execute_batch(
            "CREATE TABLE rep_attempt_spend(
                authority TEXT NOT NULL,
                resource TEXT NOT NULL,
                spent INTEGER NOT NULL CHECK(typeof(spent)='integer' AND spent>=0),
                PRIMARY KEY(authority,resource)
             ) WITHOUT ROWID;
             INSERT INTO rep_attempt_spend
                SELECT json_extract(CAST(request AS TEXT),'$.authority'),cost.key,sum(cost.value)
                FROM attempts,json_each(CAST(request AS TEXT),'$.costs') AS cost
                GROUP BY 1,2;
             CREATE TRIGGER rep_attempt_spend_insert AFTER INSERT ON attempts BEGIN
                INSERT INTO rep_attempt_spend
                  SELECT json_extract(CAST(NEW.request AS TEXT),'$.authority'),cost.key,cost.value
                  FROM json_each(CAST(NEW.request AS TEXT),'$.costs') AS cost WHERE true
                  ON CONFLICT(authority,resource) DO UPDATE SET spent=spent+excluded.spent;
             END;",
        )?;
    }
    Ok(())
}
