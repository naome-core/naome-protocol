//! Block storage for historical single-object payloads; their original content IDs remain resolvable.
use super::{ObjectAvailability, custody::BLOCK};
use crate::wire::Id;
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

pub(super) fn put(db: &Connection, id: Id, data: &[u8], access: Id) -> Result<ObjectAvailability> {
    let tx = if db.is_autocommit() {
        Some(rusqlite::Transaction::new_unchecked(
            db,
            TransactionBehavior::Immediate,
        )?)
    } else {
        None
    };
    let key = id.to_string();
    let existing: Option<(String, bool)> = db
        .query_row(
            "SELECT access,data IS NULL FROM objects WHERE id=?",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((acl, withdrawn)) = existing {
        ensure!(
            acl == access.to_string(),
            "conflicting access for immutable payload"
        );
        if withdrawn {
            return Ok(ObjectAvailability::Withdrawn);
        }
    }
    id.verify(data)?;
    db.execute(
        "INSERT OR IGNORE INTO objects VALUES(?,x'',?)",
        params![key, access.to_string()],
    )?;
    if db.execute(
        "INSERT OR IGNORE INTO archive_extents VALUES(?,?)",
        params![key, data.len() as i64],
    )? != 0
    {
        for (i, block) in data.chunks(BLOCK).enumerate() {
            db.prepare_cached("INSERT INTO archive_extent_blocks VALUES(?,?,?)")?
                .execute(params![key, i as i64, block])?;
        }
        db.execute(
            "UPDATE objects SET data=x'' WHERE id=? AND length(data)>0",
            [&key],
        )?;
    }
    if let Some(tx) = tx {
        tx.commit()?
    }
    Ok(ObjectAvailability::Available)
}
pub(super) fn get(db: &Connection, id: Id, length: usize) -> Result<Vec<u8>> {
    ensure!(length <= crate::wire::MAX_BYTES, "oversized extent");
    let mut q = db.prepare_cached(
        "SELECT ordinal,data FROM archive_extent_blocks WHERE id=? ORDER BY ordinal",
    )?;
    let mut rows = q.query([id.to_string()])?;
    let mut data = Vec::with_capacity(length);
    while let Some(row) = rows.next()? {
        let index: i64 = row.get(0)?;
        let block: Vec<u8> = row.get(1)?;
        ensure!(
            index as usize == data.len() / BLOCK
                && block.len() == (length - data.len()).min(BLOCK)
                && !block.is_empty(),
            "invalid extent block"
        );
        data.extend_from_slice(&block);
    }
    ensure!(data.len() == length, "incomplete extent");
    id.verify(&data)?;
    Ok(data)
}
