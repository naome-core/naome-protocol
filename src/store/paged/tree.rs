//! Immutable B+ tree pages keyed by ASCII protocol keys. Admission batches are sorted once.
use crate::wire::Id;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const FANOUT: usize = 32;
const MAX_PAGE: usize = 16 * 1024;
type Entry = (String, Id);

#[derive(Serialize, Deserialize)]
enum Page {
    Leaf(Vec<Entry>),
    Branch(Vec<Entry>),
}

#[derive(Default, Debug, Serialize)]
pub struct PageIo {
    pub reads: u64,
    pub read_bytes: u64,
    pub writes: u64,
    pub written_bytes: u64,
}

pub(super) struct Tree<'a> {
    pub db: &'a Connection,
    pub io: PageIo,
}

impl<'a> Tree<'a> {
    pub fn new(db: &'a Connection) -> Self {
        Self {
            db,
            io: PageIo::default(),
        }
    }
    fn read(&mut self, id: Id) -> Result<Page> {
        let bytes: Vec<u8> = self
            .db
            .prepare_cached("SELECT data FROM rep2_pages WHERE id=?")?
            .query_row([id.to_string()], |row| row.get(0))
            .context("missing inventory page")?;
        self.io.reads += 1;
        self.io.read_bytes += bytes.len() as u64;
        ensure!(bytes.len() <= MAX_PAGE, "oversized inventory page");
        id.verify(&bytes)?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    fn save(&mut self, rows: Vec<Entry>, branch: bool) -> Result<Vec<Entry>> {
        let groups = rows.len().div_ceil(FANOUT);
        let mut result = Vec::with_capacity(groups);
        let mut rows = rows.into_iter();
        for remaining in (1..=groups).rev() {
            let take = rows.len().div_ceil(remaining);
            let chunk: Vec<_> = rows.by_ref().take(take).collect();
            let first = chunk[0].0.clone();
            let page = if branch {
                Page::Branch(chunk)
            } else {
                Page::Leaf(chunk)
            };
            let bytes = serde_json::to_vec(&page)?;
            ensure!(bytes.len() <= MAX_PAGE, "oversized inventory page");
            let id = Id::blob(&bytes);
            if self
                .db
                .prepare_cached("INSERT OR IGNORE INTO rep2_pages VALUES(?,?)")?
                .execute(params![id.to_string(), bytes])?
                != 0
            {
                self.io.writes += 1;
                self.io.written_bytes += bytes.len() as u64;
            }
            result.push((first, id));
        }
        Ok(result)
    }
    pub fn apply(&mut self, root: Option<Id>, changes: BTreeMap<String, Id>) -> Result<Id> {
        ensure!(!changes.is_empty(), "empty inventory update");
        let keys: Vec<_> = changes.into_iter().collect();
        let mut row = match root {
            Some(id) => self.update(id, &keys)?,
            None => self.save(keys, false)?,
        };
        while row.len() > 1 {
            row = self.save(row, true)?;
        }
        Ok(row[0].1)
    }
    fn update(&mut self, id: Id, changes: &[Entry]) -> Result<Vec<Entry>> {
        match self.read(id)? {
            Page::Leaf(rows) => {
                let mut entries: BTreeMap<_, _> = rows.into_iter().collect();
                let old_len = entries.len();
                for (key, value) in changes {
                    if let Some(previous) = entries.insert(key.clone(), *value) {
                        ensure!(previous == *value, "conflicting immutable inventory entry");
                    }
                }
                if old_len == entries.len() {
                    return Ok(vec![(entries.first_key_value().unwrap().0.clone(), id)]);
                }
                self.save(entries.into_iter().collect(), false)
            }
            Page::Branch(children) => {
                let mut cursor = 0;
                let mut next = Vec::new();
                for (i, (first, child)) in children.iter().enumerate() {
                    let end = children.get(i + 1).map_or(changes.len(), |(bound, _)| {
                        cursor + changes[cursor..].partition_point(|(k, _)| k < bound)
                    });
                    if cursor == end {
                        next.push((first.clone(), *child));
                    } else {
                        next.extend(self.update(*child, &changes[cursor..end])?);
                    }
                    cursor = end;
                }
                if children == next {
                    return Ok(vec![(children[0].0.clone(), id)]);
                }
                self.save(next, true)
            }
        }
    }
    pub fn get(&mut self, mut root: Id, key: &str) -> Result<Option<Id>> {
        loop {
            match self.read(root)? {
                Page::Leaf(rows) => {
                    return Ok(rows
                        .binary_search_by(|(k, _)| k.as_str().cmp(key))
                        .ok()
                        .map(|i| rows[i].1));
                }
                Page::Branch(rows) => {
                    let n = rows.partition_point(|(k, _)| k.as_str() <= key);
                    if n == 0 {
                        return Ok(None);
                    }
                    root = rows[n - 1].1;
                }
            }
        }
    }
    pub fn prefix(
        &mut self,
        root: Id,
        prefix: &str,
        limit: usize,
        found: &mut Vec<Id>,
    ) -> Result<()> {
        match self.read(root)? {
            Page::Leaf(rows) => {
                for (k, id) in rows {
                    if k.starts_with(prefix) && found.len() < limit {
                        found.push(id);
                    }
                }
            }
            Page::Branch(rows) => {
                for (i, (start, id)) in rows.iter().enumerate() {
                    if found.len() >= limit
                        || (start.as_str() > prefix && !start.starts_with(prefix))
                    {
                        break;
                    }
                    if rows.get(i + 1).is_none_or(|(end, _)| end.as_str() > prefix) {
                        self.prefix(*id, prefix, limit, found)?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl std::ops::AddAssign for PageIo {
    fn add_assign(&mut self, other: Self) {
        self.reads += other.reads;
        self.read_bytes += other.read_bytes;
        self.writes += other.writes;
        self.written_bytes += other.written_bytes;
    }
}
