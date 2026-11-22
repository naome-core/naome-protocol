mod exchange;
mod search_index;
pub mod sharing;
pub use exchange::{Bundle, Packet};

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
    path::Path,
    time::Duration,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub version: u8,
    pub source: String,
    pub parents: BTreeSet<Id>,
    pub objects: BTreeSet<Id>,
}
impl Snapshot {
    pub fn id(&self) -> Result<Id> {
        Ok(Id::blob(&serde_json::to_vec(self)?))
    }
}
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
    pub fn memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }
    fn init(db: Connection) -> Result<Self> {
        db.busy_timeout(Duration::from_secs(10))?;
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON;
        CREATE TABLE IF NOT EXISTS objects(id TEXT PRIMARY KEY, data BLOB, access TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS heads(id TEXT PRIMARY KEY, manifest BLOB NOT NULL);
        CREATE TABLE IF NOT EXISTS rep_search_selection(singleton INTEGER PRIMARY KEY CHECK(singleton=1), head TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS releases(head TEXT PRIMARY KEY REFERENCES heads(id));
        CREATE TABLE IF NOT EXISTS suspended_releases(head TEXT PRIMARY KEY REFERENCES releases(head));
        CREATE VIEW IF NOT EXISTS available_releases AS SELECT head FROM releases EXCEPT SELECT head FROM suspended_releases;
        CREATE TABLE IF NOT EXISTS limits(resource TEXT PRIMARY KEY, capacity INTEGER NOT NULL CHECK(capacity>=0));
        CREATE TABLE IF NOT EXISTS revocations(authority TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS attempts(id TEXT PRIMARY KEY, request BLOB NOT NULL, state TEXT NOT NULL, witness TEXT);")?;
        let archive = Self { db };
        for bytes in [PUBLIC, READ_ONLY, PROFILE] {
            archive.blob(bytes, Id::blob(PUBLIC))?;
        }
        Ok(archive)
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
        for head in self.releases()? {
            if self.head(head)?.objects.contains(&id) {
                tx.execute(
                    "INSERT INTO suspended_releases(head) VALUES(?)",
                    [head.to_string()],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn size(&self, id: Id) -> Result<usize> {
        Ok(usize::try_from(
            self.db
                .query_row(
                    "SELECT coalesce(length(data),0) FROM objects WHERE id=?",
                    [id.to_string()],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                .unwrap_or(0),
        )?)
    }
    pub(crate) fn raw(&self, id: Id) -> Result<Option<(Vec<u8>, Id)>> {
        let item: Option<(Vec<u8>, String)> = self
            .db
            .query_row(
                "SELECT data,access FROM objects WHERE id=? AND data IS NOT NULL",
                [id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        item.map(|(b, a)| {
            id.verify(&b)?;
            Ok((b, a.parse()?))
        })
        .transpose()
    }
    pub fn resolve(&self, s: &Snapshot, id: Id, purpose: &str) -> Result<Vec<u8>> {
        self.resolve_with(s, id, |policy| allows(policy, purpose))
            .map(|(bytes, _)| bytes)
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
    fn check_access(&self, acl: Id, allows: impl FnOnce(&[u8]) -> bool) -> Result<()> {
        let (policy, parent) = self.raw(acl)?.context("unavailable access policy")?;
        ensure!(
            acl.format().is_none() && parent == Id::blob(PUBLIC) && allows(&policy),
            "use withheld"
        );
        Ok(())
    }
    pub(crate) fn may_be_notice(&self, s: &Snapshot, id: Id) -> bool {
        id.format().is_some_and(|format| {
            self.resolve(s, id, "read")
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
    fn read_with(
        &self,
        id: Id,
        mut resolve: impl FnMut(Id) -> Result<Vec<u8>>,
    ) -> Result<Document> {
        let format = id.format().context("expected record")?;
        let r = Record::decode(&resolve(id)?, format)?;
        let profile = supported_profile(r.profile).context("unsupported profile")?;
        ensure!(resolve(r.profile)? == profile, "inconsistent profile bytes");
        Document::decode(&r)
    }
    fn id_set(&self, sql: &str) -> Result<BTreeSet<Id>> {
        self.db
            .prepare(sql)?
            .query_map([], |r| r.get::<_, String>(0))?
            .map(|row| row?.parse())
            .collect()
    }

    pub fn snapshot(&self, source: &str, parents: BTreeSet<Id>) -> Result<Snapshot> {
        let s = Snapshot {
            version: 1,
            source: source.into(),
            parents,
            objects: self.id_set("SELECT id FROM objects WHERE data IS NOT NULL")?,
        };
        self.save(&s)?;
        Ok(s)
    }
    /// Seal the readable forward dependency closure of explicitly selected objects.
    pub fn selected_snapshot(
        &self,
        source: &str,
        parents: BTreeSet<Id>,
        roots: BTreeSet<Id>,
    ) -> Result<Snapshot> {
        ensure!(!roots.is_empty(), "no snapshot roots selected");
        let tx = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let mut pending: Vec<_> = roots.iter().copied().collect();
        let mut objects = roots;
        while let Some(id) = pending.pop() {
            let (bytes, access) = self
                .raw(id)?
                .context("selected object or dependency is unavailable")?;
            self.check_access(access, |policy| crate::profile::allows(policy, "read"))?;
            let record = id
                .format()
                .map(|format| Record::decode(&bytes, format))
                .transpose()?;
            // Blob access is custody metadata outside the hashed payload. Record
            // inputs also carry the profile and each ordered evidence reference.
            for dependency in std::iter::once(access).chain(record.iter().flat_map(Record::inputs))
            {
                if objects.insert(dependency) {
                    pending.push(dependency);
                }
            }
        }
        let snapshot = Snapshot {
            version: 1,
            source: source.into(),
            parents,
            objects,
        };
        self.save(&snapshot)?;
        tx.commit()?;
        Ok(snapshot)
    }

    pub fn save(&self, s: &Snapshot) -> Result<Id> {
        ensure!(s.version == 1 && !s.source.is_empty(), "invalid snapshot");
        let bytes = serde_json::to_vec(s)?;
        let id = Id::blob(&bytes);
        self.db.execute(
            "INSERT OR IGNORE INTO heads VALUES(?,?)",
            params![id.to_string(), bytes],
        )?;
        Ok(id)
    }
    pub fn head(&self, id: Id) -> Result<Snapshot> {
        let b: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT manifest FROM heads WHERE id=?",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let b = match b {
            Some(b) => b,
            None => {
                self.raw(id)?
                    .filter(|(_, acl)| *acl == Id::blob(PUBLIC))
                    .context("unknown snapshot")?
                    .0
            }
        };
        id.verify(&b)?;
        let s: Snapshot = serde_json::from_slice(&b)?;
        ensure!(
            s.version == 1 && !s.source.is_empty() && s.id()? == id,
            "invalid snapshot manifest"
        );
        Ok(s)
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
    /// Include arguments citing the target and candidate defeaters in the declared inventory.
    pub fn argument_graph(
        &self,
        s: &Snapshot,
        id: Id,
        purpose: &str,
        budget: usize,
        depth: usize,
    ) -> Result<serde_json::Value> {
        let mut arguments = vec![];
        let mut notices = BTreeMap::new();
        let mut unsearched = BTreeSet::new();
        for (n, &object) in s.objects.iter().enumerate() {
            if n >= budget {
                unsearched.insert(object);
                continue;
            }
            if object.format().is_none() {
                continue;
            }
            let Ok(d) = self.read(s, object, purpose) else {
                unsearched.insert(object);
                continue;
            };
            if d.kind == Kind::Argument && d.roles("claim").any(|c| c == id) {
                arguments.push(object);
            }
            if d.kind == Kind::Notice {
                notices.insert(object, d.inputs);
            }
        }
        let mut roots = arguments.clone();
        roots.push(id);
        let graph = self.explain(s, &roots, purpose, budget, depth);
        let contrary: Vec<_> = notices
            .iter()
            .filter(|(_, edges)| edges.iter().any(|e| graph.visited.contains(&e.target)))
            .map(|(notice, _)| notice)
            .collect();
        Ok(serde_json::json!({
            "snapshot": s.id()?,
            "source": s.source,
            "target": id,
            "arguments": arguments,
            "dependencies": graph,
            "candidate_notices": contrary,
            "unsearched": unsearched,
            "budget_per_phase": budget,
        }))
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
        ensure!(budget > 0, "positive page budget required");
        let mut ids = Vec::new();
        let mut examined = 0;
        let mut excluded = 0;
        let mut last = None;
        let mut more = false;
        for &id in s.objects.iter().filter(|id| after.is_none_or(|x| **id > x)) {
            if examined == budget {
                more = true;
                break;
            }
            examined += 1;
            last = Some(id);
            if id.format().is_none() {
                continue;
            }
            match self.read(s, id, purpose) {
                Ok(d)
                    if kind.is_none_or(|k| k == d.kind)
                        && subject.is_none_or(|t| t == d.subject)
                        && scope.is_none_or(|t| t == d.scope) =>
                {
                    ids.push(id)
                }
                Err(_) => excluded += 1,
                _ => (),
            }
        }
        Ok(Page {
            head: s.id()?,
            ids,
            after: if more { last } else { None },
            examined,
            excluded,
            ranking: "ascending content identifier",
            source: s.source.clone(),
        })
    }
}
fn check_object_bounds(bytes: &[u8], access: Id) -> Result<()> {
    ensure!(
        bytes.len() <= MAX_BYTES && access.format().is_none(),
        "invalid size or access reference"
    );
    Ok(())
}
fn validate(id: Id, b: &[u8], access: Id) -> Result<()> {
    check_object_bounds(b, access)?;
    id.verify(b)?;
    if let Some(f) = id.format() {
        ensure!(Record::decode(b, f)?.access == access, "access mismatch")
    }
    Ok(())
}
#[derive(PartialEq, Eq)]
pub(crate) enum ObjectAvailability {
    Available,
    Withdrawn,
}

fn insert(db: &Connection, id: Id, b: &[u8], access: Id) -> Result<ObjectAvailability> {
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
