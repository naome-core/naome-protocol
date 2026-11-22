use super::{normalize, terms};
use crate::{
    search::{SearchMode, SearchOptions},
    wire::Id,
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
pub(super) struct RankedPassage {
    pub(super) id: i64,
    pub(super) rank: usize,
    pub(super) similarity: Option<f32>,
    pub(super) part: Option<i64>,
    strength: f64,
}

pub(super) struct Candidate {
    pub(super) id: Id,
    pub(super) lexical: Option<RankedPassage>,
    pub(super) semantic: Option<RankedPassage>,
}

impl Candidate {
    pub(super) fn passage(&self) -> i64 {
        [self.lexical, self.semantic]
            .into_iter()
            .flatten()
            .max_by(|left, right| {
                (left.strength / left.rank as f64).total_cmp(&(right.strength / right.rank as f64))
            })
            .expect("candidate has at least one passage")
            .id
    }

    fn score(&self) -> f64 {
        [self.lexical, self.semantic]
            .into_iter()
            .flatten()
            .map(|item| item.strength / (10.0 + item.rank as f64))
            .sum()
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
    exhausted: &mut bool,
) -> Result<Vec<Candidate>> {
    let bound = (options.limit * 4).max(40);
    let mut candidates = BTreeMap::new();
    if options.mode != SearchMode::Semantic {
        for (rank, (id, passage, strength)) in lexical(db, head, query, options, bound)?
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
            semantic_passages(db, head, options, semantic, bound, exhausted)?
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
    candidates.sort_by(|left, right| {
        right
            .score()
            .total_cmp(&left.score())
            .then_with(|| left.id.cmp(&right.id))
    });
    candidates.truncate(options.limit);
    Ok(candidates)
}

fn lexical(
    db: &Connection,
    head: &str,
    query: &str,
    options: SearchOptions<'_>,
    bound: usize,
) -> Result<Vec<(Id, i64, f64)>> {
    let query = normalize(query);
    let terms = terms(&query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = db.prepare_cached(
        "WITH scored AS (
            SELECT e.object,p.id,p.location,COUNT(*) AS matches,instr(p.normalized,?3)>0 AS exact,
                   COUNT(*)*4+p.priority+2*(instr(p.normalized,?3)>0) AS score
            FROM json_each(?2) token
            JOIN rep_search_terms t ON t.head=?1 AND t.term=token.value
            JOIN rep_search_passages p ON p.id=t.passage
            JOIN rep_search_entries e ON e.id=p.entry
            WHERE (?4 IS NULL OR e.kind=?4) AND (?5 IS NULL OR e.subject=?5) AND (?6 IS NULL OR e.scope=?6)
            GROUP BY p.id
        ), ordered AS (
            SELECT object,id,score,matches,exact,ROW_NUMBER() OVER(PARTITION BY object ORDER BY score DESC,location,id) AS rank
            FROM scored
        )
        SELECT object,id,matches,exact FROM ordered WHERE rank=1 ORDER BY score DESC,object LIMIT ?7",
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
                ))
            },
        )?
        .map(|row| {
            let (object, passage, matches, exact) = row?;
            let strength = if exact {
                1.0
            } else {
                (matches / terms.len() as f64).powi(2)
            };
            Ok((object.parse()?, passage, strength))
        })
        .collect()
}

// Exact cosine ranking over the bounded authorized partition.
fn semantic_passages(
    db: &Connection,
    head: &str,
    options: SearchOptions<'_>,
    semantic: SemanticQuery<'_>,
    bound: usize,
    exhausted: &mut bool,
) -> Result<Vec<(Id, i64, i64, f32)>> {
    let mut statement = db.prepare_cached(
        "SELECT e.object,p.id,p.location,v.part,v.vector
         FROM rep_search_vectors v
         JOIN rep_search_passages p ON p.id=v.passage
         JOIN rep_search_entries e ON e.id=p.entry
         WHERE v.head=?1 AND v.model=?2
             AND (?3 IS NULL OR e.kind=?3) AND (?4 IS NULL OR e.subject=?4) AND (?5 IS NULL OR e.scope=?5)
         ORDER BY v.passage,v.part LIMIT ?6",
    )?;
    let budget = options.vector_budget.min(i64::MAX as usize - 1);
    let mut rows = statement.query(params![
        head,
        semantic.model,
        options.kind.map(|kind| kind as i64),
        options.subject,
        options.scope,
        budget as i64 + 1
    ])?;
    let mut best = BTreeMap::new();
    for examined in 0..=budget {
        let Some(row) = rows.next()? else { break };
        if examined == budget {
            *exhausted = true;
            break;
        }
        let vector = row.get_ref(4)?.as_blob()?;
        let (components, remainder) = vector.as_chunks();
        ensure!(
            remainder.is_empty() && components.len() == semantic.vectors[0].len(),
            "stored embedding dimensions disagree"
        );
        let mut similarity = f32::NEG_INFINITY;
        for query in semantic.vectors {
            let score: f32 = components
                .iter()
                .zip(query)
                .map(|(bytes, query)| f32::from_le_bytes(*bytes) * query)
                .sum();
            similarity = similarity.max(score);
        }
        ensure!(similarity.is_finite(), "invalid stored embedding");
        if similarity < semantic.minimum {
            continue;
        }
        let id: Id = row.get::<_, String>(0)?.parse()?;
        let passage: i64 = row.get(1)?;
        let location: String = row.get(2)?;
        let part: i64 = row.get(3)?;
        let previous = best
            .entry(id)
            .or_insert((passage, part, similarity, location.clone()));
        if similarity > previous.2
            || (similarity == previous.2
                && (location.as_str(), part) < (previous.3.as_str(), previous.1))
        {
            *previous = (passage, part, similarity, location);
        }
    }
    let mut best: Vec<_> = best
        .into_iter()
        .map(|(id, (passage, part, similarity, _))| (id, passage, part, similarity))
        .collect();
    best.sort_by(|left, right| {
        right
            .3
            .total_cmp(&left.3)
            .then_with(|| left.0.cmp(&right.0))
    });
    best.truncate(bound);
    Ok(best)
}
