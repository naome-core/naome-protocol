mod semantic;

use super::extract::{normalize, terms};
use crate::{
    search::{SearchMode, SearchOptions},
    wire::Id,
};
use anyhow::Result;
use rusqlite::{Connection, params};
pub(super) use semantic::VectorWork;
use semantic::semantic_passages;
use std::collections::BTreeMap;

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
    query: &str,
    options: SearchOptions<'_>,
    semantic: Option<SemanticQuery<'_>>,
    work: &mut VectorWork,
) -> Result<Vec<Candidate>> {
    let bound = (options.limit * 4).max(40);
    let mut candidates = BTreeMap::new();
    if options.mode != SearchMode::Semantic {
        for (rank, (id, passage, strength, exact_key)) in lexical(db, head, query, options, bound)?
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
            semantic_passages(db, head, options, semantic, bound, work)?
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
    query: &str,
    options: SearchOptions<'_>,
    bound: usize,
) -> Result<Vec<(Id, i64, f64, bool)>> {
    let query = normalize(query);
    let terms = terms(&query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = db.prepare_cached(
        "WITH scored AS (
            SELECT e.object,p.id,p.location,COUNT(*) AS matches,instr(p.normalized,?3)>0 AS exact,
                   p.priority>=2 AND instr(' '||p.normalized||' ', ' '||?3||' ')>0 AS exact_key,
                   COUNT(*)*4+p.priority+2*(instr(p.normalized,?3)>0) AS score
            FROM json_each(?2) token
            JOIN rep_search_terms t ON t.term=token.value
            JOIN rep_search_passages p ON p.id=t.passage
            JOIN rep_search_entries e ON e.id=p.entry
            JOIN rep_search_visibility visible ON visible.entry=e.id AND visible.head=?1
            WHERE (?4 IS NULL OR e.kind=?4) AND (?5 IS NULL OR e.subject=?5) AND (?6 IS NULL OR e.scope=?6)
            GROUP BY p.id
        ), ordered AS (
            SELECT object,id,score,matches,exact,exact_key,ROW_NUMBER() OVER(PARTITION BY object ORDER BY exact_key DESC,score DESC,location,id) AS rank
            FROM scored
        )
        SELECT object,id,matches,exact,exact_key FROM ordered WHERE rank=1 ORDER BY exact_key DESC,score DESC,object LIMIT ?7",
    )?;
    statement
        .query_map(
            params![
                head,
                serde_json::to_string(&terms)?,
                query,
                options.kind.map(|kind| kind as i64),
                options.subject,
                options.scope,
                bound as i64
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, bool>(4)?,
                ))
            },
        )?
        .map(|row| {
            let (object, passage, matches, exact, exact_key) = row?;
            let strength = if exact {
                1.0
            } else {
                (matches / terms.len() as f64).powi(2)
            };
            Ok((object.parse()?, passage, strength, exact_key))
        })
        .collect()
}
