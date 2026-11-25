//! Block storage for immutable single-object payloads, including unpublished exchange data.
use super::{ObjectAvailability, custody::BLOCK};
use crate::wire::Id;
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

// This keeps small values in one indexed row on the default SQLite page size.
const INLINE_BYTES: usize = 512;

fn has_inline_data(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('archive_extents') WHERE name='inline_data')",
        [],
        |row| row.get(0),
    )?)
}

pub(super) fn upgrade(db: &Connection) -> Result<()> {
    if !has_inline_data(db)? {
        db.execute_batch("ALTER TABLE archive_extents ADD COLUMN inline_data BLOB;")?;
    }
    Ok(())
}

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
    store(db, &key, data)?;
    db.prepare_cached("UPDATE objects SET data=x'' WHERE id=? AND length(data)>0")?
        .execute([&key])?;
    if let Some(tx) = tx {
        tx.commit()?
    }
    Ok(ObjectAvailability::Available)
}
/// Write validated bytes by content ID inside the caller's write transaction.
pub(super) fn store(db: &Connection, key: &str, data: &[u8]) -> Result<()> {
    let inline = (data.len() <= INLINE_BYTES).then_some(data);
    let added = db
        .prepare_cached(
            "INSERT OR IGNORE INTO archive_extents(id,length,inline_data) VALUES(?,?,?)",
        )?
        .execute(params![key, data.len() as i64, inline])?;
    if added != 0 && inline.is_none() {
        let mut insert = db.prepare_cached("INSERT INTO archive_extent_blocks VALUES(?,?,?)")?;
        for (i, block) in data.chunks(BLOCK).enumerate() {
            insert.execute(params![key, i as i64, block])?;
        }
    }
    Ok(())
}

pub(super) fn remove(db: &Connection, key: &str) -> Result<()> {
    if db
        .prepare_cached("DELETE FROM archive_extents WHERE id=?")?
        .execute([key])?
        != 0
    {
        db.prepare_cached("DELETE FROM archive_extent_blocks WHERE id=?")?
            .execute([key])?;
    }
    Ok(())
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
        let block = row.get_ref(1)?.as_blob()?;
        ensure!(
            index as usize == data.len() / BLOCK
                && block.len() == (length - data.len()).min(BLOCK)
                && !block.is_empty(),
            "invalid extent block"
        );
        data.extend_from_slice(block);
    }
    ensure!(data.len() == length, "incomplete extent");
    id.verify(&data)?;
    Ok(data)
}
