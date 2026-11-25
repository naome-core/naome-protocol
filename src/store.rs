mod exchange;
mod extents;
mod inventory;
mod upgrade;
pub use inventory::Manifest;
pub mod custody;
mod search_index;
pub mod sharing;
mod snapshot;
pub use exchange::{Bundle, ExchangePage, ExchangeProgress, MemberProof, Packet};
#[cfg(feature = "cli")]
pub(crate) use search_index::IndexPreparation;
pub use snapshot::Snapshot;
pub(crate) use snapshot::{InventoryReader, ManifestEncoding};

use crate::profile::TrainingAccess;
use crate::{
    profile::{
        Document, Kind, MEASUREMENT_PROFILE, PROFILE, PUBLIC, READ_ONLY, allows, supported_profile,
    },
    wire::{Format, Id, MAX_BYTES, Record},
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::Bound::{Excluded, Unbounded},
    path::Path,
    time::Duration,
};

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct Closure {
    pub visited: BTreeSet<Id>,
    pub frontier: BTreeMap<Id, String>,
}
#[derive(Debug, Serialize)]
pub struct Page {
    pub head: Id,
    pub ids: Vec<Id>,
    pub after: Option<Id>,
    pub examined: usize,
    pub excluded: usize,
    pub ranking: &'static str,
    pub source: String,
}
pub struct Archive {
    pub(crate) db: Connection,
}
#[derive(Serialize)]
pub struct ArchiveStatus {
    pub full_snapshot: Option<WorkingSnapshot>,
    pub search_head: Option<Id>,
    pub search_pinned: bool,
    pub pending_objects: u64,
}

#[derive(Serialize)]
pub struct WorkingSnapshot {
    pub id: Id,
    pub source: String,
    pub withdrawals_since: i64,
}
impl Archive {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            if let Err(e) = opts.open(path)
                && e.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(e.into());
            }
        }
        Self::init(Connection::open(path)?)
    }
    /// Open an existing, current archive without initialization or migration.
    pub fn open_for_read(path: impl AsRef<Path>) -> Result<Self> {
        let reader = Self::custody_reader(path)?;
        let has_layout: bool = reader.db.query_row(
            "SELECT count(*)=2 FROM sqlite_schema
             WHERE type='table' AND name IN ('archive_layout','archive_member_access')",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            has_layout
                && reader.db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM archive_layout)",
                    [],
                    |row| row.get::<_, bool>(0),
                )?,
            "archive needs initialization or upgrade; use nao init or nao migrate-archive"
        );
        Ok(reader)
    }
    pub fn memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }
    fn init(db: Connection) -> Result<Self> {
        db.busy_timeout(Duration::from_secs(10))?;
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON;")?;
        let archive = Self { db };
        let tx = rusqlite::Transaction::new_unchecked(
            &archive.db,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS objects(id TEXT PRIMARY KEY, data BLOB, access TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS heads(id TEXT PRIMARY KEY, manifest BLOB NOT NULL);
        CREATE TABLE IF NOT EXISTS rep_imported_heads(head TEXT PRIMARY KEY REFERENCES heads(id));
        CREATE TABLE IF NOT EXISTS rep_search_selection(singleton INTEGER PRIMARY KEY CHECK(singleton=1), head TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS releases(head TEXT PRIMARY KEY REFERENCES heads(id));
        CREATE TABLE IF NOT EXISTS suspended_releases(head TEXT PRIMARY KEY REFERENCES releases(head));
        CREATE VIEW IF NOT EXISTS available_releases AS SELECT head FROM releases EXCEPT SELECT head FROM suspended_releases;
        CREATE TABLE IF NOT EXISTS limits(resource TEXT PRIMARY KEY, capacity INTEGER NOT NULL CHECK(capacity>=0));
        CREATE TABLE IF NOT EXISTS revocations(authority TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS attempts(id TEXT PRIMARY KEY, request BLOB NOT NULL, state TEXT NOT NULL, witness TEXT);")?;
        archive.init_custody()?;
        for bytes in [PUBLIC, READ_ONLY, PROFILE] {
            archive.blob(bytes, Id::blob(PUBLIC))?;
        }
        archive.upgrade_catalog()?;
        tx.commit()?;
        Ok(archive)
    }
    /// All reads observe one custody state, including when called inside an existing transaction.
    pub(crate) fn with_read_view<T>(&self, read: impl FnOnce() -> Result<T>) -> Result<T> {
        if !self.db.is_autocommit() {
            return read();
        }
        let transaction = self.db.unchecked_transaction()?;
        let result = read()?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn blob(&self, bytes: &[u8], access: Id) -> Result<Id> {
        self.store_blob(bytes, access).map(|(id, _)| id)
    }

    pub(crate) fn store_blob(&self, bytes: &[u8], access: Id) -> Result<(Id, ObjectAvailability)> {
        check_object_bounds(bytes, access)?;
        let id = Id::blob(bytes);
        Ok((id, insert(&self.db, id, bytes, access)?))
    }
    pub fn document(&self, d: &Document, f: Format, access: Id) -> Result<Id> {
        let record = d.encode(access)?;
        if record.profile == Id::blob(MEASUREMENT_PROFILE) && self.raw(record.profile)?.is_none() {
            self.blob(MEASUREMENT_PROFILE, Id::blob(PUBLIC))?;
        }
        self.record(&record, f)
    }
    pub fn record(&self, r: &Record, f: Format) -> Result<Id> {
        let b = r.encode(f);
        let id = Id::record(f, &b);
        self.deposit(id, &b, r.access)?;
        Ok(id)
    }
    pub fn deposit(&self, id: Id, bytes: &[u8], access: Id) -> Result<()> {
        validate(id, bytes, access)?;
        insert(&self.db, id, bytes, access).map(|_| ())
    }
    pub fn withdraw(&self, id: Id) -> Result<()> {
        ensure!(
            id.format().is_none()
                && ![Id::blob(PUBLIC), Id::blob(READ_ONLY)].contains(&id)
                && supported_profile(id).is_none(),
            "withdraw a non-bootstrap payload"
        );
        let tx = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        ensure!(
            tx.execute("UPDATE objects SET data=NULL WHERE id=?", [id.to_string()])? == 1,
            "unknown payload"
        );
        tx.execute(
            "DELETE FROM archive_extent_blocks WHERE id=?",
            [id.to_string()],
        )?;
        tx.execute("INSERT OR IGNORE INTO suspended_releases SELECT head FROM archive_release_members WHERE member=?",[id.to_string()])?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn size(&self, id: Id) -> Result<usize> {
        Ok(usize::try_from(
            self.db
                .prepare_cached("SELECT CASE WHEN o.data IS NULL THEN 0 ELSE coalesce(r.length,e.length,length(o.data),0) END FROM objects o LEFT JOIN custody_records r ON o.id=r.id
                   LEFT JOIN archive_extents e ON e.id=o.id WHERE o.id=?")?
                .query_row([id.to_string()], |r| r.get::<_, i64>(0))
                .optional()?
                .unwrap_or(0),
        )?)
    }
    pub(crate) fn raw(&self, id: Id) -> Result<Option<(Vec<u8>, Id)>> {
        let item: Option<(Vec<u8>, String,Option<i64>)> = self
            .db
            .prepare_cached("SELECT CASE WHEN r.id IS NULL THEN o.data ELSE substr(p.data,r.start+1,r.length) END,o.access,e.length FROM objects o LEFT JOIN custody_records r ON o.id=r.id
                   LEFT JOIN custody_packs p ON p.id=r.pack
                   LEFT JOIN archive_extents e ON e.id=o.id WHERE o.id=? AND o.data IS NOT NULL")?
            .query_row([id.to_string()], |r| Ok((r.get(0)?, r.get(1)?,r.get(2)?)))
            .optional()?;
        item.map(|(b, a, length)| {
            let b = if let Some(n) = length {
                extents::get(&self.db, id, usize::try_from(n)?)?
            } else {
                id.verify(&b)?;
                b
            };
            Ok((b, a.parse()?))
        })
        .transpose()
    }
    pub fn resolve(&self, s: &Snapshot, id: Id, purpose: &str) -> Result<Vec<u8>> {
        self.resolve_with(s, id, |policy| allows(policy, purpose))
            .map(|(bytes, _)| bytes)
    }

    pub(crate) fn resolve_local(&self, id: Id) -> Result<Vec<u8>> {
        let (bytes, access) = self.raw(id)?.context("unavailable")?;
        self.check_access(access, |policy| allows(policy, "read"))?;
        Ok(bytes)
    }
    pub(crate) fn resolve_for_training(
        &self,
        s: &Snapshot,
        id: Id,
        access: &TrainingAccess<'_>,
    ) -> Result<Vec<u8>> {
        self.resolve_with(s, id, |policy| access.allows(policy))
            .map(|(bytes, _)| bytes)
    }
    fn resolve_with(
        &self,
        s: &Snapshot,
        id: Id,
        allows: impl FnOnce(&[u8]) -> bool,
    ) -> Result<(Vec<u8>, Id)> {
        ensure!(s.objects.contains(&id), "outside snapshot");
        let (b, acl) = self.raw(id)?.context("unavailable")?;
        ensure!(s.objects.contains(&acl), "missing access descriptor");
        self.check_access(acl, allows)?;
        Ok((b, acl))
    }
    /// Check presence and read permission without materializing the payload.
    pub(crate) fn readable(&self, s: &Snapshot, id: Id) -> Result<()> {
        self.readable_with(id, |key| Ok(s.objects.contains(&key)))
    }

    fn readable_with(&self, id: Id, mut contains: impl FnMut(Id) -> Result<bool>) -> Result<()> {
        ensure!(contains(id)?, "outside snapshot");
        let acl: String = self.db.query_row(
            "SELECT access FROM objects WHERE id = ?1 AND data IS NOT NULL",
            [id.to_string()],
            |row| row.get(0),
        )?;
        let acl = acl.parse()?;
        ensure!(contains(acl)?, "missing access descriptor");
        self.check_access(acl, |policy| crate::profile::allows(policy, "read"))
    }
    pub(crate) fn check_access(&self, acl: Id, allows: impl FnOnce(&[u8]) -> bool) -> Result<()> {
        let (policy, parent) = self.raw(acl)?.context("unavailable access policy")?;
        ensure!(
            acl.format().is_none() && parent == Id::blob(PUBLIC) && allows(&policy),
            "use withheld"
        );
        Ok(())
    }
    pub(crate) fn may_be_notice(&self, s: &Snapshot, id: Id, purpose: &str) -> bool {
        id.format().is_some_and(|format| {
            self.resolve(s, id, purpose)
                .ok()
                .and_then(|bytes| Record::decode(&bytes, format).ok())
                .is_none_or(|record| {
                    supported_profile(record.profile).is_none()
                        || record.kind == Kind::Notice as u64
                })
        })
    }
    pub fn read(&self, s: &Snapshot, id: Id, purpose: &str) -> Result<Document> {
        self.read_with(id, |id| self.resolve(s, id, purpose))
    }
    /// An executor authority, decision and attestation must carry the public descriptor.
    pub(crate) fn read_public(&self, s: &Snapshot, id: Id) -> Result<Document> {
        self.read_with(id, |input| {
            self.resolve_with(s, input, |descriptor| descriptor == PUBLIC)
                .map(|(bytes, _)| bytes)
        })
    }
    pub(crate) fn read_for_training(
        &self,
        s: &Snapshot,
        id: Id,
        access: &TrainingAccess<'_>,
    ) -> Result<Document> {
        self.read_with(id, |id| self.resolve_for_training(s, id, access))
    }
    pub(crate) fn read_with(
        &self,
        id: Id,
        mut resolve: impl FnMut(Id) -> Result<Vec<u8>>,
    ) -> Result<Document> {
        let format = id.format().context("expected record")?;
        let bytes = resolve(id)?;
        Self::decode_document(format, &bytes, resolve)
    }

    fn decode_document(
        format: Format,
        bytes: &[u8],
        resolve_profile: impl FnOnce(Id) -> Result<Vec<u8>>,
    ) -> Result<Document> {
        let r = Record::decode(bytes, format)?;
        let profile = supported_profile(r.profile).context("unsupported profile")?;
        ensure!(
            resolve_profile(r.profile)? == profile,
            "inconsistent profile bytes"
        );
        Document::decode(&r)
    }
    fn id_set(&self, sql: &str) -> Result<BTreeSet<Id>> {
        self.db
            .prepare(sql)?
            .query_map([], |r| r.get::<_, String>(0))?
            .map(|row| row?.parse())
            .collect()
    }

    pub fn explain(
        &self,
        s: &Snapshot,
        roots: &[Id],
        purpose: &str,
        budget: usize,
        depth: usize,
    ) -> Closure {
        self.dependency_closure(roots, budget, depth, None, |id| {
            self.resolve(s, id, purpose)
        })
    }
    /// The argument consumes the stored value of this reading. Its own explicit
    /// premises still apply, even when they also appear in the reading's provenance.
    pub(crate) fn explain_recorded_reading(
        &self,
        s: &Snapshot,
        argument: Id,
        reading: Id,
        budget: usize,
    ) -> Closure {
        self.dependency_closure(&[argument], budget, 48, Some(reading), |id| {
            self.resolve(s, id, "read")
        })
    }
    pub(crate) fn explain_for_training(
        &self,
        s: &Snapshot,
        roots: &[Id],
        access: &TrainingAccess<'_>,
    ) -> Closure {
        self.dependency_closure(roots, s.objects.len(), 48, None, |id| {
            self.resolve_for_training(s, id, access)
        })
    }
    fn dependency_closure(
        &self,
        roots: &[Id],
        budget: usize,
        depth: usize,
        value_only: Option<Id>,
        resolve: impl Fn(Id) -> Result<Vec<u8>>,
    ) -> Closure {
        let mut result = Closure::default();
        let mut queue: VecDeque<_> = roots.iter().map(|id| (*id, 0)).collect();
        let mut examined = 0;
        while let Some((id, d)) = queue.pop_front() {
            if result.visited.contains(&id) || result.frontier.contains_key(&id) {
                continue;
            }
            if examined >= budget || d > depth {
                result.frontier.insert(id, "search limit".into());
                continue;
            }
            examined += 1;
            let next = (|| -> Result<Vec<Id>> {
                let bytes = resolve(id)?;
                let Some(f) = id.format() else {
                    return Ok(vec![]);
                };
                let r = Record::decode(&bytes, f)?;
                Ok(if Some(id) == value_only {
                    vec![r.profile, r.access]
                } else {
                    r.inputs().collect()
                })
            })();
            match next {
                Ok(inputs) => {
                    result.visited.insert(id);
                    queue.extend(inputs.into_iter().map(|i| (i, d + 1)));
                }
                Err(e) => {
                    result.frontier.insert(id, e.to_string());
                }
            }
        }
        result
    }
    #[allow(clippy::too_many_arguments)]
    pub fn discover(
        &self,
        s: &Snapshot,
        kind: Option<Kind>,
        subject: Option<&str>,
        scope: Option<&str>,
        purpose: &str,
        budget: usize,
        after: Option<Id>,
    ) -> Result<Page> {
        let mut page = Page::start(s.id()?, &s.source, budget)?;
        let bound = after.map_or(Unbounded, Excluded);
        let finished = s.objects.range((bound, Unbounded)).all(|&id| {
            page.examine(
                id,
                budget,
                || self.read(s, id, purpose),
                kind,
                subject,
                scope,
            )
        });
        Ok(page.finish(finished))
    }

    /// Enumerate the requested part of the authenticated inventory without hydrating the cut.
    #[allow(clippy::too_many_arguments)]
    pub fn discover_head(
        &self,
        head: Id,
        kind: Option<Kind>,
        subject: Option<&str>,
        scope: Option<&str>,
        purpose: &str,
        budget: usize,
        after: Option<Id>,
    ) -> Result<Page> {
        self.with_read_view(|| {
            let manifest = self.active_manifest(head)?;
            let mut page = Page::start(manifest.id()?, &manifest.source, budget)?;
            let mut tree = custody::tree::Tree::new(&self.db);
            let finished = manifest.scan_after(&self.db, after, |id| {
                Ok(page.examine(
                    id,
                    budget,
                    || {
                        let format = id.format().context("expected record")?;
                        let (bytes, _) =
                            self.resolve_inventory_member(&manifest, id, purpose, &mut tree)?;
                        Self::decode_document(format, &bytes, |profile| {
                            Ok(self
                                .resolve_member_in(&manifest, profile, purpose, &mut tree)?
                                .0)
                        })
                    },
                    kind,
                    subject,
                    scope,
                ))
            })?;
            Ok(page.finish(finished))
        })
    }
}

impl Page {
    fn start(head: Id, source: &str, budget: usize) -> Result<Self> {
        ensure!(budget > 0, "positive page budget required");
        Ok(Self {
            head,
            ids: Vec::new(),
            after: None,
            examined: 0,
            excluded: 0,
            ranking: "ascending content identifier",
            source: source.to_owned(),
        })
    }
    fn examine(
        &mut self,
        id: Id,
        budget: usize,
        read: impl FnOnce() -> Result<Document>,
        kind: Option<Kind>,
        subject: Option<&str>,
        scope: Option<&str>,
    ) -> bool {
        if self.examined == budget {
            return false;
        }
        self.examined += 1;
        self.after = Some(id);
        if id.format().is_some() {
            match read() {
                Ok(d)
                    if kind.is_none_or(|k| k == d.kind)
                        && subject.is_none_or(|t| t == d.subject)
                        && scope.is_none_or(|t| t == d.scope) =>
                {
                    self.ids.push(id)
                }
                Err(_) => self.excluded += 1,
                _ => {}
            }
        }
        true
    }
    fn finish(mut self, complete: bool) -> Self {
        if complete {
            self.after = None;
        }
        self
    }
}

fn check_object_bounds(bytes: &[u8], access: Id) -> Result<()> {
    ensure!(
        bytes.len() <= MAX_BYTES && access.format().is_none(),
        "invalid size or access reference"
    );
    Ok(())
}
fn validate(id: Id, b: &[u8], access: Id) -> Result<Option<Record>> {
    check_object_bounds(b, access)?;
    id.verify(b)?;
    if let Some(f) = id.format() {
        let record = Record::decode(b, f)?;
        ensure!(record.access == access, "access mismatch");
        return Ok(Some(record));
    }
    Ok(None)
}
#[derive(PartialEq, Eq)]
pub(crate) enum ObjectAvailability {
    Available,
    Withdrawn,
}

fn insert(db: &Connection, id: Id, b: &[u8], access: Id) -> Result<ObjectAvailability> {
    if id.format().is_some()
        && db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM custody_records WHERE id=? AND access=?)")?
            .query_row(params![id.to_string(), access.to_string()], |r| {
                r.get::<_, bool>(0)
            })?
    {
        return Ok(ObjectAvailability::Available);
    }
    if id.format().is_none() && b.len() > custody::BLOCK {
        return extents::put(db, id, b, access);
    }
    let added = db.execute(
        "INSERT OR IGNORE INTO objects VALUES(?,?,?)",
        params![id.to_string(), b, access.to_string()],
    )?;
    if added != 0 {
        return Ok(ObjectAvailability::Available);
    }

    let (same, withdrawn): (bool, bool) = db.query_row(
        "SELECT access=? AND (data IS NULL OR data=?), data IS NULL FROM objects WHERE id=?",
        params![access.to_string(), b, id.to_string()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if !same {
        bail!("conflicting access or bytes for immutable object {id}");
    }
    Ok(if withdrawn {
        ObjectAvailability::Withdrawn
    } else {
        ObjectAvailability::Available
    })
}
