//! Account for each supplied witness inside the import transaction. Temporary indices keep
//! the verifier's memory independent of stream length and disappear on rollback or success.
use super::{Manifest, Page, install_page};
use crate::{store::custody::tree::Tree, wire::Id};
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};

pub(in crate::store) struct Witnesses<'a>(&'a Connection);

impl<'a> Witnesses<'a> {
    pub fn begin(db: &'a Connection) -> Result<Self> {
        db.execute_batch(
            "CREATE TEMP TABLE incoming_pages(id TEXT PRIMARY KEY) WITHOUT ROWID;
            CREATE TEMP TABLE incoming_horizons(id TEXT PRIMARY KEY, header BLOB) WITHOUT ROWID;",
        )?;
        Ok(Self(db))
    }

    pub fn install(&self, id: Id, page: &Page) -> Result<bool> {
        let inserted = install_page(self.0, id, page)?;
        self.0
            .prepare_cached("INSERT OR IGNORE INTO temp.incoming_pages VALUES(?)")?
            .execute([id.to_string()])?;
        Ok(inserted)
    }

    pub fn link(&self, id: Id, bytes: &[u8]) -> Result<()> {
        if Manifest::from_packet(id, bytes).is_some() {
            self.0
                .prepare_cached("INSERT OR IGNORE INTO temp.incoming_horizons VALUES(?,?)")?
                .execute(params![id.to_string(), bytes])?;
        }
        Ok(())
    }

    pub fn used(&self, id: Id) -> Result<()> {
        self.0
            .prepare_cached("DELETE FROM temp.incoming_pages WHERE id=?")?
            .execute([id.to_string()])?;
        Ok(())
    }

    pub fn walk(&self, manifest: &Manifest) -> Result<()> {
        let mut tree = Tree::new(self.0);
        manifest.walk(
            |id| {
                self.used(id)?;
                tree.read(id)
            },
            |_| Ok(()),
        )
    }

    fn unused(&self) -> Result<bool> {
        Ok(self.0.query_row(
            "SELECT EXISTS(SELECT 1 FROM temp.incoming_pages)",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn finish(self) -> Result<()> {
        if self.unused()? {
            let mut query = self
                .0
                .prepare("SELECT header FROM temp.incoming_horizons")?;
            let mut rows = query.query([])?;
            while let Some(row) = rows.next()? {
                let bytes: Vec<u8> = row.get(0)?;
                let linked: Manifest = serde_json::from_slice(&bytes)?;
                // An optional historical view may be unavailable. Only a complete walk
                // justifies retaining its sidecar pages.
                self.0.execute_batch("SAVEPOINT linked_witness")?;
                if self.walk(&linked).is_err() {
                    self.0.execute_batch("ROLLBACK TO linked_witness")?;
                }
                self.0.execute_batch("RELEASE linked_witness")?;
                if !self.unused()? {
                    break;
                }
            }
            ensure!(!self.unused()?, "unreferenced inventory witness");
        }
        self.0
            .execute_batch("DROP TABLE temp.incoming_horizons; DROP TABLE temp.incoming_pages;")?;
        Ok(())
    }
}
