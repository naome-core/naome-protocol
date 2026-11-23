//! Public snapshot commitment. The custody proof can travel separately from its inventory.
use crate::wire::Id;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub source: String,
    pub parents: BTreeSet<Id>,
    pub inventory: Id,
    pub count: u64,
    pub custody: Id,
}
impl Manifest {
    pub fn bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }
    pub fn id(&self) -> Result<Id> {
        Ok(Id::blob(&self.bytes()?))
    }
    pub fn verify(&self, id: Id) -> Result<()> {
        ensure!(
            !self.source.is_empty()
                && self.source.len() <= 256
                && self.parents.len() <= 64
                && self.inventory.blob_digest().is_some()
                && self.custody.blob_digest().is_some()
                && self.id()? == id,
            "invalid snapshot commitment"
        );
        Ok(())
    }
}

pub use super::custody::tree::Page;
use super::custody::tree::Tree;
use anyhow::Context;
use rusqlite::{Connection, params};
use std::collections::BTreeMap;

pub type Pages = BTreeMap<Id, Page>;

fn visit(
    id: Id,
    depth: usize,
    upper: Option<&str>,
    lower: Option<&str>,
    page: &mut impl FnMut(Id) -> Result<Page>,
    member: &mut impl FnMut(Id) -> Result<()>,
) -> Result<()> {
    ensure!(depth <= 32, "inventory depth limit");
    let node = page(id)?;
    let (leaf, entries) = match &node {
        Page::Leaf(v) => (true, v),
        Page::Branch(v) => (false, v),
    };
    ensure!(
        entries.len() <= 32 && (!entries.is_empty() || leaf && depth == 0),
        "invalid inventory page size"
    );
    ensure!(
        entries.windows(2).all(|w| w[0].0 < w[1].0)
            && lower.is_none_or(|low| entries.first().is_some_and(|(k, _)| k == low))
            && upper.is_none_or(|high| entries.last().is_none_or(|(k, _)| k.as_str() < high)),
        "invalid inventory key bounds"
    );
    for (i, (key, value)) in entries.iter().enumerate() {
        if leaf {
            ensure!(
                key == &value.to_string(),
                "invalid or repeated inventory member"
            );
            member(*value)?;
        } else {
            visit(
                *value,
                depth + 1,
                entries.get(i + 1).map(|e| e.0.as_str()).or(upper),
                Some(key),
                page,
                member,
            )?;
        }
    }
    Ok(())
}
impl Manifest {
    pub(super) fn walk(
        &self,
        mut page: impl FnMut(Id) -> Result<Page>,
        mut member: impl FnMut(Id) -> Result<()>,
    ) -> Result<()> {
        let mut count = 0u64;
        visit(
            self.inventory,
            0,
            None,
            None,
            &mut |id| {
                let p = page(id)?;
                id.verify(&serde_json::to_vec(&p)?)?;
                Ok(p)
            },
            &mut |id| {
                ensure!(count < self.count, "inventory exceeds declared size");
                count += 1;
                member(id)
            },
        )?;
        ensure!(count == self.count, "inventory count mismatch");
        Ok(())
    }
    fn members(&self, page: impl FnMut(Id) -> Result<Page>) -> Result<BTreeSet<Id>> {
        let mut ids = BTreeSet::new();
        self.walk(page, |id| {
            ids.insert(id);
            Ok(())
        })?;
        Ok(ids)
    }
    pub(super) fn read(&self, db: &Connection) -> Result<(BTreeSet<Id>, Pages)> {
        let mut tree = Tree::new(db);
        let mut pages = Pages::new();
        let objects = self.members(|id| {
            let p = tree.read(id)?;
            pages.insert(id, p.clone());
            Ok(p)
        })?;
        Ok((objects, pages))
    }
    pub(super) fn witness(&self, pages: &Pages) -> Result<(BTreeSet<Id>, Pages)> {
        let mut used = Pages::new();
        let ids = self.members(|id| {
            let p = pages
                .get(&id)
                .context("missing inventory witness page")?
                .clone();
            used.insert(id, p.clone());
            Ok(p)
        })?;
        Ok((ids, used))
    }
}
pub(super) fn install(db: &Connection, pages: &Pages) -> Result<()> {
    for (id, page) in pages {
        let bytes = serde_json::to_vec(page)?;
        ensure!(bytes.len() <= 16384, "oversized witness page");
        id.verify(&bytes)?;
        db.prepare_cached("INSERT OR IGNORE INTO custody_pages VALUES(?,?)")?
            .execute(params![id.to_string(), bytes])?;
    }
    Ok(())
}
