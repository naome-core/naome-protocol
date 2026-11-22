use super::SemanticQuery;
use crate::{search::SearchOptions, wire::Id};
use anyhow::{Result, ensure};
use rusqlite::{Connection, Row, params};
use std::{
    cmp::{Ordering, Reverse},
    collections::{BTreeMap, BinaryHeap},
};

/// Count multiply-adds spent comparing the query with eligible vector components.
#[derive(Default)]
pub(in super::super) struct VectorWork {
    pub(in super::super) products: usize,
    pub(in super::super) bounded: bool,
}

const SCOPE: &str = "
    FROM rep_search_vectors v
    JOIN rep_search_passages p ON p.id=v.passage
    JOIN rep_search_entries e ON e.id=p.entry
    JOIN rep_search_visibility visible ON visible.entry=e.id AND visible.head=?1
    WHERE v.model=?2
      AND (?3 IS NULL OR e.kind=?3) AND (?4 IS NULL OR e.subject=?4) AND (?5 IS NULL OR e.scope=?5)";

struct Scope<'a> {
    db: &'a Connection,
    head: &'a str,
    options: SearchOptions<'a>,
    model: &'a str,
}

type Match = (Id, i64, i64, f32);

#[derive(Clone, Copy)]
struct Coarse {
    similarity: f32,
    passage: i64,
    part: i64,
}

impl Ord for Coarse {
    fn cmp(&self, other: &Self) -> Ordering {
        self.similarity
            .total_cmp(&other.similarity)
            .then_with(|| (other.passage, other.part).cmp(&(self.passage, self.part)))
    }
}
impl PartialOrd for Coarse {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Coarse {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for Coarse {}

pub(super) fn semantic_passages(
    db: &Connection,
    head: &str,
    options: SearchOptions<'_>,
    semantic: SemanticQuery<'_>,
    bound: usize,
    work: &mut VectorWork,
) -> Result<Vec<Match>> {
    let dimensions = semantic.vectors[0].len();
    let query_count = semantic.vectors.len().min(options.vector_budget);
    let queries: Vec<_> = (0..query_count)
        .map(|position| {
            semantic.vectors[position * semantic.vectors.len() / query_count].as_slice()
        })
        .collect();
    let budget = options.vector_budget.saturating_mul(dimensions);
    let exact_cost = queries.len() * dimensions;
    let scope = Scope {
        db,
        head,
        options,
        model: semantic.model,
    };
    let count = scope.count()?;
    work.bounded = query_count < semantic.vectors.len() || count > budget / exact_cost;
    let mut best = BTreeMap::new();
    let mut decoded = Vec::with_capacity(dimensions);
    let mut exact = |row: &Row<'_>| -> Result<()> {
        let values = decode(row, dimensions, &mut decoded)?;
        let score = maximum_dot(values, &queries);
        ensure!(score.is_finite(), "invalid stored embedding");
        if score >= semantic.minimum {
            let id: Id = row.get::<_, String>(0)?.parse()?;
            let passage: i64 = row.get(1)?;
            let location: String = row.get(2)?;
            let part: i64 = row.get(3)?;
            let previous = best
                .entry(id)
                .or_insert_with(|| (passage, part, score, location.clone()));
            if score > previous.2
                || (score == previous.2
                    && (location.as_str(), part) < (previous.3.as_str(), previous.1))
            {
                *previous = (passage, part, score, location);
            }
        }
        Ok(())
    };

    if count <= budget / exact_cost {
        scope.visit(dimensions, count, count, &mut exact)?;
        work.products = count * exact_cost;
    } else {
        let coarse_budget = (budget - exact_cost) / 2;
        let width = (coarse_budget / query_count / count).clamp(1, dimensions.min(128));
        let samples = count.min(coarse_budget / (query_count * width));
        if samples == 0 {
            // Tiny caller-supplied budgets can only afford sampled exact comparisons.
            let samples = count.min(budget / exact_cost);
            scope.visit(dimensions, count, samples, &mut exact)?;
            work.products = samples * exact_cost;
        } else {
            let coarse_products = samples * query_count * width;
            let finalists = samples.min((budget - coarse_products) / exact_cost);
            let shortlist = shortlist(&scope, &queries, width, count, samples, finalists)?;
            let mut statement = db.prepare_cached(
                "SELECT e.object,p.id,p.location,v.part,v.vector FROM rep_search_vectors v
                 JOIN rep_search_passages p ON p.id=v.passage JOIN rep_search_entries e ON e.id=p.entry
                 WHERE v.model=? AND v.passage=? AND v.part=?",
            )?;
            for item in &shortlist {
                let mut rows = statement.query(params![semantic.model, item.passage, item.part])?;
                if let Some(row) = rows.next()? {
                    exact(row)?;
                }
            }
            work.products = coarse_products + shortlist.len() * exact_cost;
        }
    }

    let mut best: Vec<_> = best
        .into_iter()
        .map(|(id, (passage, part, score, _))| (id, passage, part, score))
        .collect();
    let compare = |left: &Match, right: &Match| {
        right
            .3
            .total_cmp(&left.3)
            .then_with(|| left.0.cmp(&right.0))
    };
    if best.len() > bound {
        best.select_nth_unstable_by(bound, compare);
        best.truncate(bound);
    }
    best.sort_by(compare);
    Ok(best)
}

fn shortlist(
    scope: &Scope<'_>,
    queries: &[&[f32]],
    width: usize,
    count: usize,
    samples: usize,
    finalists: usize,
) -> Result<Vec<Coarse>> {
    let prefixes: Vec<_> = queries
        .iter()
        .map(|query| unit_prefix(query, width))
        .collect();
    let query_refs: Vec<_> = prefixes.iter().map(Vec::as_slice).collect();
    let mut decoded = Vec::with_capacity(width);
    let mut best = BinaryHeap::with_capacity(finalists);
    scope.visit(width, count, samples, |row| {
        let values = decode(row, width, &mut decoded)?;
        let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
        let similarity = if norm > 0.0 {
            maximum_dot(values, &query_refs) / norm
        } else {
            0.0
        };
        ensure!(similarity.is_finite(), "invalid projected embedding");
        let item = Coarse {
            similarity,
            passage: row.get(1)?,
            part: row.get(3)?,
        };
        if best.len() < finalists {
            best.push(Reverse(item));
        } else if best.peek().is_some_and(|worst| worst.0 < item) {
            *best.peek_mut().expect("shortlist is full") = Reverse(item);
        }
        Ok(())
    })?;
    Ok(best.into_iter().map(|item| item.0).collect())
}

impl Scope<'_> {
    fn count(&self) -> Result<usize> {
        let count: i64 = self.db.query_row(
            &format!("SELECT count(*) {SCOPE}"),
            params![
                self.head,
                self.model,
                self.options.kind.map(|kind| kind as i64),
                self.options.subject,
                self.options.scope
            ],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(count)?)
    }

    fn visit(
        &self,
        width: usize,
        count: usize,
        samples: usize,
        mut visit: impl FnMut(&Row<'_>) -> Result<()>,
    ) -> Result<()> {
        if samples == 0 {
            return Ok(());
        }
        let sql = format!(
            "SELECT e.object,p.id,p.location,v.part,substr(v.vector,1,?6) {SCOPE} ORDER BY p.id,v.part",
        );
        let mut statement = self.db.prepare_cached(&sql)?;
        let mut rows = statement.query(params![
            self.head,
            self.model,
            self.options.kind.map(|kind| kind as i64),
            self.options.subject,
            self.options.scope,
            (width * 4) as i64
        ])?;
        let mut available = 0;
        let mut visited = 0;
        while let Some(row) = rows.next()? {
            if available
                == (((2 * visited as u128 + 1) * count as u128) / (2 * samples as u128)) as usize
            {
                visit(row)?;
                visited += 1;
                if visited == samples {
                    break;
                }
            }
            available += 1;
        }
        Ok(())
    }
}

fn decode<'a>(row: &Row<'_>, width: usize, buffer: &'a mut Vec<f32>) -> Result<&'a [f32]> {
    let (chunks, remainder) = row.get_ref(4)?.as_blob()?.as_chunks();
    ensure!(
        remainder.is_empty() && chunks.len() == width,
        "stored embedding dimensions disagree"
    );
    buffer.clear();
    buffer.extend(chunks.iter().map(|bytes| f32::from_le_bytes(*bytes)));
    Ok(buffer)
}

fn unit_prefix(vector: &[f32], width: usize) -> Vec<f32> {
    let prefix = &vector[..width];
    let norm = prefix.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        prefix.iter().map(|value| value / norm).collect()
    } else {
        prefix.to_vec()
    }
}

fn maximum_dot(vector: &[f32], queries: &[&[f32]]) -> f32 {
    queries
        .iter()
        .map(|query| {
            vector
                .iter()
                .zip(*query)
                .map(|(left, right)| left * right)
                .sum::<f32>()
        })
        .fold(f32::NEG_INFINITY, f32::max)
}
