//! Root-scoped dependency routing. Scientific adjudication requires its own profile and evidence.
use super::*;
use std::collections::VecDeque;

#[derive(Serialize, Deserialize)]
pub struct Federation {
    pub heads: BTreeSet<Id>,
}
impl Federation {
    pub fn new(heads: BTreeSet<Id>) -> Result<Self> {
        ensure!((1..=64).contains(&heads.len()), "invalid federation size");
        Ok(Self { heads })
    }
    pub fn id(&self) -> Result<Id> {
        Ok(Id::blob(&serde_json::to_vec(self)?))
    }
}
#[derive(Serialize)]
pub struct Review {
    pub correction_search: bool,
    pub notices: BTreeSet<Id>,
    pub view: Id,
    pub heads: BTreeSet<Id>,
    pub evaluator: &'static str,
    pub target: Id,
    pub budget: usize,
    pub standing: &'static str,
    pub dependents: BTreeSet<Id>,
    pub reason: Option<String>,
    pub io: PageIo,
}

impl Archive {
    pub(super) fn check_withdrawals(
        &self,
        h: &Head,
        tree: &mut Tree<'_>,
        budget: usize,
    ) -> Result<()> {
        // Custody is live: a pinned inventory alone does not prove continued availability.
        let mut statement = self
            .db
            .prepare("SELECT id FROM objects WHERE data IS NULL ORDER BY id LIMIT ?")?;
        let mut rows = statement.query([(budget + 1) as i64])?;
        let mut checked = 0;
        while let Some(row) = rows.next()? {
            checked += 1;
            let id: Id = row.get::<_, String>(0)?.parse()?;
            ensure!(
                checked <= budget && tree.get(h.tree, &object(id))?.is_none(),
                "withdrawal or incomplete custody coverage"
            );
        }
        Ok(())
    }
}
impl Federation {
    /// The caller supplies the custody archive for each pinned head (these can be separate files).
    pub fn review(&self, stores: &[(&Archive, Id)], target: Id, budget: usize) -> Result<Review> {
        self.trace(stores, target, budget, false)
    }
    /// Find candidate correction notices for a target and its declared transitive premises.
    pub fn corrections(
        &self,
        stores: &[(&Archive, Id)],
        target: Id,
        budget: usize,
    ) -> Result<Review> {
        self.trace(stores, target, budget, true)
    }
    fn trace(
        &self,
        stores: &[(&Archive, Id)],
        target: Id,
        budget: usize,
        corrections: bool,
    ) -> Result<Review> {
        ensure!((1..=100_000).contains(&budget), "invalid review budget");
        let mut report = Review {
            correction_search: corrections,
            notices: BTreeSet::new(),
            view: self.id()?,
            heads: self.heads.clone(),
            evaluator: "rep/declared-dependencies",
            target,
            budget,
            standing: "open",
            dependents: BTreeSet::new(),
            reason: None,
            io: PageIo::default(),
        };
        ensure!(stores.len() <= 64, "too many sources");
        let mut ordered = stores.to_vec();
        ordered.sort_by_key(|(_, id)| *id);
        let stores = ordered.as_slice();
        // Acquire one SQLite read transaction per distinct connection. A vector of pinned local cuts
        // is meaningful across independent labs; a simultaneous distributed wall clock is unnecessary.
        let mut guards = Vec::new();
        for &(archive, _) in stores {
            if archive.db.is_autocommit() {
                guards.push(archive.db.unchecked_transaction()?);
            }
        }
        let mut inventories = Vec::new();
        let result = (|| -> Result<()> {
            ensure!(
                (1..=64).contains(&self.heads.len())
                    && stores.len() == self.heads.len()
                    && stores.iter().map(|(_, id)| *id).collect::<BTreeSet<_>>() == self.heads,
                "missing pinned source"
            );

            let mut present = false;
            let mut unresolved = BTreeSet::new();
            for &(store, id) in stores {
                let h = store.custody_head(id)?;
                inventories.push((h, Tree::new(&store.db)));
                let (h, tree) = inventories.last_mut().unwrap();
                ensure!(
                    !h.restricted && h.opaque == 0 && h.missing == h.unresolved.len() as u64,
                    "restricted, opaque or incomplete coverage"
                );
                unresolved.extend(h.unresolved.iter().copied());
                store.check_withdrawals(h, tree, budget)?;
                present |= tree.get(h.tree, &object(target))?.is_some();
            }
            ensure!(present, "target outside the selected view");
            let mut pending = VecDeque::from([target]);
            let mut visited = BTreeSet::from([target]);
            let mut remaining = budget;
            for id in unresolved {
                let mut found = false;
                for (h, tree) in &mut inventories {
                    ensure!(remaining > 0, "cross-lab coverage budget exhausted");
                    remaining -= 1;
                    if tree.get(h.tree, &object(id))?.is_some() {
                        found = true;
                        break;
                    }
                }
                ensure!(found, "unresolved declared dependency");
            }
            while let Some(parent) = pending.pop_front() {
                for (h, tree) in &mut inventories {
                    ensure!(remaining > 0, "dependency review budget exhausted");
                    remaining -= 1;
                    let mut ids = Vec::new();
                    if corrections {
                        tree.prefix(h.tree, &format!("c/{parent}/"), remaining + 1, &mut ids)?;
                        ensure!(ids.len() <= remaining, "correction review budget exhausted");
                        remaining -= ids.len();
                        report.notices.extend(ids.drain(..));
                    }
                    let direction = if corrections { "f" } else { "r" };
                    tree.prefix(
                        h.tree,
                        &format!("{direction}/{parent}/"),
                        remaining + 1,
                        &mut ids,
                    )?;
                    ensure!(ids.len() <= remaining, "dependency review budget exhausted");
                    remaining -= ids.len();
                    for id in ids {
                        if visited.insert(id) {
                            if !corrections {
                                report.dependents.insert(id);
                            }
                            pending.push_back(id);
                        }
                    }
                }
            }

            Ok(())
        })();
        for (_, tree) in inventories {
            report.io += tree.io;
        }
        match result {
            Ok(()) => {
                report.standing = if report.dependents.is_empty() && report.notices.is_empty() {
                    "outside"
                } else {
                    "review"
                }
            }
            Err(e) => {
                report.reason = Some(e.to_string());
                report.dependents.clear();
                report.notices.clear();
            }
        }
        for guard in guards {
            guard.commit()?;
        }
        Ok(report)
    }
}

impl Review {
    /// Canonical compact observation. The sorted result digest bounds receipt size irrespective of fan-out.
    pub fn receipt(&self) -> Result<Vec<u8>> {
        let matches = if self.correction_search {
            &self.notices
        } else {
            &self.dependents
        };
        Ok(serde_json::to_vec(&serde_json::json!({
            "evaluator":self.evaluator,"view":self.view,"heads":self.heads,"target":self.target,
            "correction_search":self.correction_search,"budget":self.budget,"standing":self.standing,
            "reason":self.reason,"result_count":matches.len(),"result_digest":Id::blob(&serde_json::to_vec(matches)?)
        }))?)
    }
}

impl Archive {
    /// Private custody metadata; excluded from member replication.
    pub fn custody_save_receipt(&self, review: &Review) -> Result<Id> {
        let data = review.receipt()?;
        let id = Id::blob(&data);
        self.db.execute(
            "INSERT OR IGNORE INTO custody_receipts VALUES(?,?)",
            params![id.to_string(), data],
        )?;
        Ok(id)
    }
    pub fn custody_receipt(&self, id: Id) -> Result<Vec<u8>> {
        let data: Vec<u8> = self.db.query_row(
            "SELECT data FROM custody_receipts WHERE id=?",
            [id.to_string()],
            |r| r.get(0),
        )?;
        id.verify(&data)?;
        Ok(data)
    }
}
