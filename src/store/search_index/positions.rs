//! Optional order statistics for prepared, unfiltered semantic searches.
//! Preparation advances in canonical object, section, passage location and part order.
use super::Archive;
use anyhow::Result;
use rusqlite::{OptionalExtension, TransactionBehavior, params};

pub(super) fn ready(db: &rusqlite::Connection, head: &str, model: &str) -> Result<bool> {
    let mut query = db.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM rep_search_samples
         WHERE head=? AND model=? AND complete AND NOT invalid)",
    )?;
    Ok(query.query_row(params![head, model], |row| row.get(0))?)
}

struct Cursor {
    object: String,
    section: i64,
    location: String,
    part: i64,
    count: i64,
}

impl Archive {
    pub(super) fn prepare_positions(
        &self,
        head: &str,
        model: &str,
        mut budget: usize,
    ) -> Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM rep_search_samples WHERE head=? AND model=? AND invalid",
            params![head, model],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO rep_search_samples(head,model) VALUES(?,?)",
            params![head, model],
        )?;
        tx.execute("DELETE FROM rep_search_samples WHERE id IN (SELECT id FROM rep_search_samples ORDER BY id DESC LIMIT -1 OFFSET 4)",[])?;
        let (sample, mut c, complete) = tx.query_row(
            "SELECT id,object,section,location,part,count,complete FROM rep_search_samples
             WHERE head=? AND model=?",
            params![head, model],
            |row| {
                let cursor = Cursor {
                    object: row.get(1)?,
                    section: row.get(2)?,
                    location: row.get(3)?,
                    part: row.get(4)?,
                    count: row.get(5)?,
                };
                Ok((row.get::<_, i64>(0)?, cursor, row.get::<_, bool>(6)?))
            },
        )?;
        if complete {
            return tx.commit().map_err(Into::into);
        }
        let sets: Vec<i64> = tx
            .prepare_cached("SELECT set_id FROM rep_search_head_sets WHERE head=?")?
            .query_map([head], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut finished = false;
        while budget > 0 {
            let mut entry = None;
            let mut seek = tx.prepare_cached(
                "SELECT object,section,entry FROM rep_search_set_entries
                WHERE set_id=?1 AND (object,section)>=(?2,?3) ORDER BY object,section LIMIT 1",
            )?;
            for set in &sets {
                let candidate = seek
                    .query_row(params![set, c.object, c.section], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    })
                    .optional()?;
                if let Some(candidate) = candidate
                    && entry.as_ref().is_none_or(|current| &candidate < current)
                {
                    entry = Some(candidate);
                }
            }
            let Some((object, section, id)) = entry else {
                finished = true;
                break;
            };
            if c.object != object || c.section != section {
                c.location.clear();
                c.part = -1;
            }
            c.object = object;
            c.section = section;
            let mut keys=tx.prepare_cached("SELECT p.id,p.location,v.part FROM rep_search_passages p INDEXED BY rep_search_passages_entry
               CROSS JOIN rep_search_vectors v INDEXED BY rep_search_vector_keys ON v.passage=p.id AND v.model=?2
               WHERE p.entry=?1 AND p.location>=?3 AND (p.location,v.part)>(?3,?4)
               ORDER BY p.location,v.part LIMIT ?5")?;
            let mut rows = keys.query(params![
                id,
                model,
                c.location,
                c.part,
                budget.min(i64::MAX as usize) as i64
            ])?;
            let mut insert =
                tx.prepare_cached("INSERT INTO rep_search_positions VALUES(?,?,?,?)")?;
            while let Some(row) = rows.next()? {
                let passage: i64 = row.get(0)?;
                c.location = row.get(1)?;
                c.part = row.get(2)?;
                insert.execute(params![sample, c.count, passage, c.part])?;
                c.count += 1;
                budget -= 1;
            }
            if budget > 0 {
                // Entry keys are nonnegative sections. Seek past this one on the next pass.
                c.section += 1;
                c.location.clear();
                c.part = -1;
                budget -= 1;
            }
        }
        tx.execute("UPDATE rep_search_samples SET object=?,section=?,location=?,part=?,count=?,complete=? WHERE id=?",
            params![c.object,c.section,c.location,c.part,c.count,finished,sample])?;
        tx.commit()?;
        Ok(())
    }
}
