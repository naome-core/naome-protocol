mod semantic;

use super::lexical::{normalize, terms};
use crate::{
    search::{SearchMode, SearchOptions},
    wire::Id,
};
use anyhow::Result;
use rusqlite::{Connection, params};
pub(super) use semantic::VectorWork;
use semantic::semantic_passages;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy)]
pub(super) struct RankedPassage {
    pub(super) id: i64,
    pub(super) rank: usize,
    pub(super) similarity: Option<f32>,
    pub(super) part: Option<i64>,
    strength: f64,
    exact_key: bool,
}

pub(super) struct Candidate {
    pub(super) id: Id,
    pub(super) lexical: Option<RankedPassage>,
    pub(super) semantic: Option<RankedPassage>,
}

impl Candidate {
    fn preferred(&self) -> (u8, RankedPassage) {
        if let Some(item) = self.lexical.filter(|item| item.exact_key) {
            return (0, item);
        }
        if let Some(item) = self.semantic {
            return (1, item);
        }
        (2, self.lexical.expect("candidate has a matching passage"))
    }

    pub(super) fn passage(&self) -> i64 {
        self.preferred().1.id
    }

    fn order(&self) -> (u8, usize, Id) {
        let (tier, item) = self.preferred();
        (tier, item.rank, self.id)
    }
}

pub(super) struct SemanticQuery<'a> {
    pub(super) model: &'a str,
    pub(super) vectors: &'a [Vec<f32>],
    pub(super) minimum: f32,
}

pub(super) fn rank(
    db: &Connection,
    head: &str,
    queries: &[&str],
    options: SearchOptions<'_>,
    semantic: Option<SemanticQuery<'_>>,
    work: &mut VectorWork,
    entry_allowed: &mut dyn FnMut(i64) -> bool,
) -> Result<Vec<Candidate>> {
    let bound = (options.limit * 4).max(40);
    let mut candidates = BTreeMap::new();
    if options.mode != SearchMode::Semantic {
        for (rank, (id, passage, strength, exact_key)) in
            lexical(db, head, queries, options, bound, entry_allowed)?
                .into_iter()
                .enumerate()
        {
            candidates.insert(
                id,
                Candidate {
                    id,
                    lexical: Some(RankedPassage {
                        id: passage,
                        rank: rank + 1,
                        similarity: None,
                        part: None,
                        exact_key,
                        strength: if options.mode == SearchMode::Hybrid {
                            strength
                        } else {
                            1.0
                        },
                    }),
                    semantic: None,
                },
            );
        }
    }
    if let Some(semantic) = semantic {
        for (rank, (id, passage, part, similarity)) in
            semantic_passages(db, head, options, semantic, bound, work, entry_allowed)?
                .into_iter()
                .enumerate()
        {
            candidates
                .entry(id)
                .or_insert(Candidate {
                    id,
                    lexical: None,
                    semantic: None,
                })
                .semantic = Some(RankedPassage {
                id: passage,
                rank: rank + 1,
                similarity: Some(similarity),
                part: Some(part),
                strength: 1.0,
                exact_key: false,
            });
        }
    }
    let mut candidates: Vec<_> = candidates
        .into_values()
        .filter(|candidate| {
            options.mode != SearchMode::Hybrid
                || candidate.semantic.is_some()
                || candidate.lexical.is_some_and(|item| item.strength >= 0.25)
        })
        .collect();
    candidates.sort_by_key(Candidate::order);
    candidates.truncate(options.limit);
    Ok(candidates)
}

fn lexical(
    db: &Connection,
    head: &str,
    queries: &[&str],
    options: SearchOptions<'_>,
    bound: usize,
    entry_allowed: &mut dyn FnMut(i64) -> bool,
) -> Result<Vec<(Id, i64, f64, bool)>> {
    let queries: Vec<_> = queries
        .iter()
        .filter_map(|raw| {
            let query = normalize(raw);
            let tokens = terms(&query);
            (!tokens.is_empty()).then(|| {
                serde_json::json!({
                    "text": query, "raw": raw, "tokens": tokens
                })
            })
        })
        .collect();
    if queries.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = db.prepare_cached(
        "WITH queries AS (
            SELECT key,json_extract(value,'$.text') AS text,json_extract(value,'$.raw') AS raw,
                json_extract(value,'$.tokens') AS tokens,json_array_length(value,'$.tokens') AS size
            FROM json_each(?2)
        ), scored AS (
            SELECT e.object,e.id AS entry,p.id,p.location,
                   1.0*COUNT(*)/q.size AS fraction,
                   instr(p.normalized,q.text)>0 AS exact,
                   p.priority>=2 AND instr(' '||p.normalized||' ', ' '||q.text||' ')>0 AS exact_key,
                   p.priority>=2 AND instr(' '||p.text||' ', ' '||q.raw||' ')>0 AS exact_original,
                   COUNT(*)*4+p.priority+2*(instr(p.normalized,q.text)>0) AS score
            FROM queries q JOIN json_each(q.tokens) token
            JOIN rep_search_terms t ON t.term=token.value
            JOIN rep_search_passages p ON p.id=t.passage
            JOIN rep_search_entries e ON e.id=p.entry
            JOIN rep_search_visible visible ON visible.entry=e.id AND visible.head=?1
            WHERE (?3 IS NULL OR e.kind=?3) AND (?4 IS NULL OR e.subject=?4) AND (?5 IS NULL OR e.scope=?5)
            GROUP BY q.key,p.id
        )
        SELECT object,entry,id,fraction,exact,exact_key FROM scored
        ORDER BY exact_key DESC,exact_original DESC,score DESC,object,
                 exact DESC,fraction DESC,location,id",
    )?;
    let mut rows = statement.query(params![
        head,
        serde_json::to_string(&queries)?,
        options.kind.map(|kind| kind as i64),
        options.subject,
        options.scope,
    ])?;
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    while selected.len() < bound
        && let Some(row) = rows.next()?
    {
        let object = row.get::<_, String>(0)?.parse()?;
        if !seen.contains(&object) && entry_allowed(row.get(1)?) {
            let fraction: f64 = row.get(3)?;
            let strength = if row.get(4)? { 1.0 } else { fraction.powi(2) };
            selected.push((object, row.get(2)?, strength, row.get(5)?));
            seen.insert(object);
        }
    }
    Ok(selected)
}
