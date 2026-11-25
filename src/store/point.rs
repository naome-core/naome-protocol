//! Admission provenance for payloads received only through a point disclosure.
use super::{Archive, MEASUREMENT_PROFILE, PROFILE, PUBLIC, READ_ONLY};
use crate::wire::Id;
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use std::borrow::Cow;

fn table_exists(db: &Connection, name: &str) -> Result<bool> {
    Ok(db
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?)")?
        .query_row([name], |row| row.get(0))?)
}

pub(super) fn upgrade(db: &Connection) -> Result<()> {
    if table_exists(db, "archive_point_only")? {
        let has_legacy_serial: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('archive_point_only')
             WHERE name='legacy_serial')",
            [],
            |row| row.get(0),
        )?;
        if !has_legacy_serial {
            db.execute_batch(
                "ALTER TABLE archive_point_only ADD COLUMN legacy_serial INTEGER;
                 UPDATE archive_point_only SET legacy_serial=(
                     SELECT serial FROM archive_deposits WHERE id=archive_point_only.id
                 );
                 CREATE INDEX archive_point_legacy_deposit ON archive_point_only(legacy_serial)
                 WHERE legacy_serial IS NOT NULL;",
            )?;
        }
        return Ok(());
    }
    db.execute_batch(
        "CREATE TABLE archive_point_only(
             id TEXT PRIMARY KEY NOT NULL,legacy_serial INTEGER
         ) WITHOUT ROWID;
         CREATE INDEX archive_point_legacy_deposit ON archive_point_only(legacy_serial)
         WHERE legacy_serial IS NOT NULL;
         DROP TRIGGER IF EXISTS archive_catalog_append;
         CREATE TRIGGER archive_catalog_append AFTER INSERT ON objects
         WHEN NOT EXISTS(SELECT 1 FROM archive_point_only WHERE id=NEW.id)
         BEGIN INSERT OR IGNORE INTO archive_deposits(id) VALUES(NEW.id); END;",
    )?;
    // Older archives do not say whether a point-disclosed object was also
    // independently admitted. Keep its bytes, and require fresh admission.
    db.execute(
        &format!(
            "INSERT OR IGNORE INTO archive_point_only(id,legacy_serial)
             SELECT p.id,d.serial FROM ({}) p LEFT JOIN archive_deposits d ON d.id=p.id",
            legacy_catalog(db)?
        ),
        [],
    )?;
    Ok(())
}

fn legacy_catalog(db: &Connection) -> Result<String> {
    let bootstrap = [PUBLIC, READ_ONLY, PROFILE, MEASUREMENT_PROFILE]
        .map(|bytes| format!("'{}'", Id::blob(bytes)))
        .join(",");
    let mut sources = Vec::new();
    for table in ["archive_point_access", "archive_member_access"] {
        if table_exists(db, table)? {
            sources.push(format!(
                "SELECT p.member AS id FROM {table} p JOIN objects o ON o.id=p.member
                 WHERE p.access=o.access AND p.member NOT IN ({bootstrap})"
            ));
        }
    }
    if sources.is_empty() {
        return Ok("SELECT CAST(NULL AS TEXT) AS id WHERE 0".into());
    }
    Ok(sources.join(" UNION ALL "))
}

pub(super) fn catalog(db: &Connection) -> Result<Cow<'static, str>> {
    if table_exists(db, "archive_point_only")? {
        return Ok(Cow::Borrowed("SELECT id FROM archive_point_only"));
    }
    Ok(Cow::Owned(legacy_catalog(db)?))
}

pub(super) fn visible_in_search(db: &Connection, head: &str) -> Result<bool> {
    let points = catalog(db)?;
    Ok(db
        .prepare_cached(&format!(
            "SELECT EXISTS(SELECT 1 FROM ({points}) p
           CROSS JOIN rep_search_sources s ON s.source=p.id
           CROSS JOIN rep_search_visible v ON v.entry=s.entry WHERE v.head=?)"
        ))?
        .query_row([head], |row| row.get(0))?)
}

pub(super) fn mark_new(db: &Connection, id: Id) -> Result<()> {
    db.prepare_cached(
        "INSERT OR IGNORE INTO archive_point_only(id)
         SELECT ?1 WHERE NOT EXISTS(SELECT 1 FROM objects WHERE id=?1)",
    )?
    .execute([id.to_string()])?;
    Ok(())
}

pub(super) fn promote(db: &Connection, id: &str) -> Result<()> {
    if db
        .prepare_cached("DELETE FROM archive_point_only WHERE id=?")?
        .execute([id])?
        != 0
    {
        db.prepare_cached(
            "INSERT INTO archive_deposits(serial,id)
             VALUES((SELECT coalesce(max(serial),0)+1 FROM archive_deposits),?)
             ON CONFLICT(id) DO UPDATE SET serial=excluded.serial",
        )?
        .execute([id])?;
        if table_exists(db, "rep_search_revision")? {
            db.execute("UPDATE rep_search_revision SET value=value+1", [])?;
            db.prepare_cached("UPDATE rep_search_waiting SET ready=1 WHERE source=?")?
                .execute([id])?;
        }
    }
    Ok(())
}

pub(super) fn promote_staged(db: &Connection, head: &str, verified: bool) -> Result<()> {
    let has_points: bool = db
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM archive_point_only)")?
        .query_row([], |row| row.get(0))?;
    if !has_points {
        return Ok(());
    }
    let admitted = db.execute(
        "WITH admitted AS MATERIALIZED (
             SELECT q.id, (SELECT coalesce(max(serial),0) FROM archive_deposits)
                          + row_number() OVER (ORDER BY q.id) AS serial
             FROM archive_exchange_staged p JOIN archive_point_only q ON q.id=p.id
             JOIN objects o ON o.id=p.id
             WHERE p.head=?1 AND p.verified=?2 AND o.data IS NOT NULL
         )
         INSERT INTO archive_deposits(serial,id)
         SELECT serial,id FROM admitted WHERE true
         ON CONFLICT(id) DO UPDATE SET serial=excluded.serial",
        params![head, verified],
    )?;
    if admitted == 0 {
        return Ok(());
    }
    if table_exists(db, "rep_search_revision")? {
        db.execute("UPDATE rep_search_revision SET value=value+1", [])?;
        db.execute(
            "UPDATE rep_search_waiting SET ready=1 WHERE source IN (
                 SELECT q.id FROM archive_exchange_staged p JOIN archive_point_only q ON q.id=p.id
                 JOIN objects o ON o.id=p.id WHERE p.head=? AND p.verified=? AND o.data IS NOT NULL
             )",
            params![head, verified],
        )?;
    }
    db.execute(
        "DELETE FROM archive_point_only WHERE id IN (
             SELECT p.id FROM archive_exchange_staged p JOIN objects o ON o.id=p.id
             WHERE p.head=? AND p.verified=? AND o.data IS NOT NULL
         )",
        params![head, verified],
    )?;
    Ok(())
}

impl Archive {
    pub(super) fn point_only(&self, id: Id) -> Result<bool> {
        match self
            .db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM archive_point_only WHERE id=?)")
        {
            Ok(mut lookup) => return Ok(lookup.query_row([id.to_string()], |row| row.get(0))?),
            Err(error) if table_exists(&self.db, "archive_point_only")? => return Err(error.into()),
            Err(_) => {}
        }
        // Read-only opening must also honor the conservative upgrade rule.
        let points = legacy_catalog(&self.db)?;
        Ok(self
            .db
            .prepare_cached(&format!(
                "SELECT EXISTS(SELECT 1 FROM ({points}) WHERE id=?)"
            ))?
            .query_row([id.to_string()], |row| row.get(0))?)
    }

    pub(super) fn require_point_scope(&self, id: Id, access: Id, head: Option<Id>) -> Result<()> {
        if self.point_only(id)? {
            let head = head.ok_or_else(|| {
                anyhow::anyhow!("point disclosure requires independent admission for this use")
            })?;
            ensure!(
                self.point_access(head, id)? == Some(access),
                "point disclosure is outside this head"
            );
        }
        Ok(())
    }
}
