//! Immutable B+ tree pages keyed by ASCII protocol keys. Admission batches are sorted once.
use crate::wire::Id;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
};

const FANOUT: usize = 32;
const MAX_PAGE: usize = 16 * 1024;
type Entry = (String, Id);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Page {
    Leaf(Vec<Entry>),
    Branch(Vec<Entry>),
}

#[derive(Default, Debug, Serialize)]
pub struct PageIo {
    pub reads: u64,
    pub read_bytes: u64,
    pub writes: u64,
    pub written_bytes: u64,
    pub backing_reads: u64,
    pub backing_read_bytes: u64,
    pub backing_written_bytes: u64,
}

const PATCH: u8 = 2;
const MAX_DEPTH: u8 = 4;
const COMPRESSED: u8 = 3;

/// Physical encoding only. Inventory identity always commits the canonical JSON.
pub(in crate::store) fn encode_page(bytes: Vec<u8>) -> Vec<u8> {
    if bytes.len() >= 64 {
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&bytes, 1);
        if compressed.len() + 1 < bytes.len() {
            let mut stored = Vec::with_capacity(compressed.len() + 1);
            stored.push(COMPRESSED);
            stored.extend_from_slice(&compressed);
            return stored;
        }
    }
    bytes
}

pub(in crate::store) struct Tree<'a> {
    pub db: &'a Connection,
    pub io: PageIo,
    // Per-operation immutable pages. Each encoded page is at most MAX_PAGE bytes.
    cache: BTreeMap<Id, Rc<Page>>,
    order: VecDeque<Id>,
}

impl<'a> Tree<'a> {
    pub fn new(db: &'a Connection) -> Self {
        Self {
            db,
            io: PageIo::default(),
            cache: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }
    fn backing(&mut self, id: Id, hops: u8) -> Result<Vec<u8>> {
        let stored: Vec<u8> = self
            .db
            .prepare_cached("SELECT data FROM custody_pages WHERE id=?")?
            .query_row([id.to_string()], |row| row.get(0))
            .context("missing inventory page")?;
        ensure!(stored.len() <= MAX_PAGE, "oversized backing page");
        self.io.backing_reads += 1;
        self.io.backing_read_bytes += stored.len() as u64;
        let bytes = if stored.first() == Some(&COMPRESSED) {
            miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&stored[1..], MAX_PAGE)
                .map_err(|_| anyhow::anyhow!("invalid compressed inventory page"))?
        } else if stored.first() == Some(&PATCH) {
            ensure!(
                hops < MAX_DEPTH && stored.len() >= 37,
                "invalid backing chain"
            );
            let base_id = Id::from_blob_digest(stored[1..33].try_into()?);
            let base = self.backing(base_id, hops + 1)?;
            let prefix = u16::from_be_bytes(stored[33..35].try_into()?) as usize;
            let suffix = u16::from_be_bytes(stored[35..37].try_into()?) as usize;
            ensure!(
                prefix + suffix <= base.len() && prefix + suffix + stored.len() - 37 <= MAX_PAGE,
                "invalid backing splice"
            );
            let mut bytes = Vec::with_capacity(prefix + suffix + stored.len() - 37);
            bytes.extend_from_slice(&base[..prefix]);
            bytes.extend_from_slice(&stored[37..]);
            bytes.extend_from_slice(&base[base.len() - suffix..]);
            bytes
        } else {
            stored
        };
        id.verify(&bytes)?;
        Ok(bytes)
    }
    pub(in crate::store) fn read(&mut self, id: Id) -> Result<Page> {
        let bytes = self.backing(id, 0)?;
        self.io.reads += 1;
        self.io.read_bytes += bytes.len() as u64;
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
            let logical_size = bytes.len();
            let stored = encode_page(bytes);
            if self
                .db
                .prepare_cached("INSERT OR IGNORE INTO custody_pages VALUES(?,?)")?
                .execute(params![id.to_string(), stored])?
                != 0
            {
                self.io.writes += 1;
                self.io.backing_written_bytes += stored.len() as u64;
                self.io.written_bytes += logical_size as u64;
            }
            result.push((first, id));
        }
        Ok(result)
    }
    pub fn apply(&mut self, root: Option<Id>, changes: BTreeMap<String, Id>) -> Result<Id> {
        if changes.is_empty() {
            if let Some(id) = root {
                return Ok(id);
            }
            let bytes = serde_json::to_vec(&Page::Leaf(Vec::new()))?;
            let id = Id::blob(&bytes);
            self.db.execute(
                "INSERT OR IGNORE INTO custody_pages VALUES(?,?)",
                params![id.to_string(), bytes],
            )?;
            return Ok(id);
        }
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
        let page = self.read(id)?;
        match page {
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
    fn cached(&mut self, id: Id) -> Result<Rc<Page>> {
        if let Some(page) = self.cache.get(&id) {
            return Ok(Rc::clone(page));
        }
        let page = Rc::new(self.read(id)?);
        if self.cache.len() == 64 {
            let first = self.order.pop_front().expect("page cache order");
            self.cache.remove(&first);
        }
        self.order.push_back(id);
        self.cache.insert(id, Rc::clone(&page));
        Ok(page)
    }

    /// Resolve sorted, possibly absent keys in one descent per matching child.
    pub(super) fn lookup_many(
        &mut self,
        root: Option<Id>,
        keys: BTreeSet<String>,
    ) -> Result<BTreeMap<String, Id>> {
        let mut found = BTreeMap::new();
        if let Some(root) = root {
            self.lookup_slice(root, &keys.into_iter().collect::<Vec<_>>(), 0, &mut found)?;
        }
        Ok(found)
    }
    fn lookup_slice(
        &mut self,
        id: Id,
        keys: &[String],
        depth: usize,
        found: &mut BTreeMap<String, Id>,
    ) -> Result<()> {
        ensure!(depth <= 32, "tree depth limit");
        if keys.is_empty() {
            return Ok(());
        }
        match self.cached(id)?.as_ref() {
            Page::Leaf(rows) => {
                for key in keys {
                    if let Ok(i) = rows.binary_search_by(|(k, _)| k.cmp(key)) {
                        found.insert(key.clone(), rows[i].1);
                    }
                }
            }
            Page::Branch(rows) => {
                for (i, (start, child)) in rows.iter().enumerate() {
                    let lo = keys.partition_point(|k| k < start);
                    let hi = rows
                        .get(i + 1)
                        .map_or(keys.len(), |(end, _)| keys.partition_point(|k| k < end));
                    if lo < hi {
                        self.lookup_slice(*child, &keys[lo..hi], depth + 1, found)?;
                    }
                }
            }
        }
        Ok(())
    }
    pub fn get(&mut self, mut root: Id, key: &str) -> Result<Option<Id>> {
        for _ in 0..=32 {
            match self.cached(root)?.as_ref() {
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
        anyhow::bail!("tree depth limit")
    }
    pub fn prefix(
        &mut self,
        root: Id,
        prefix: &str,
        limit: usize,
        found: &mut Vec<Id>,
    ) -> Result<()> {
        match self.cached(root)?.as_ref() {
            Page::Leaf(rows) => {
                for (k, id) in rows {
                    if k.starts_with(prefix) && found.len() < limit {
                        found.push(*id);
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
        self.backing_reads += other.backing_reads;
        self.backing_read_bytes += other.backing_read_bytes;
        self.backing_written_bytes += other.backing_written_bytes;
    }
}
