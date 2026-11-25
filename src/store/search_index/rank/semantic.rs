use super::SemanticQuery;
use crate::{search::SearchOptions, wire::Id};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Row, Statement, params};
use std::{
    cmp::{Ordering, Reverse},
    collections::{BTreeMap, BinaryHeap},
};

/// Count multiply-adds spent comparing the query with eligible vector components.
#[derive(Default)]
pub(in super::super) struct VectorWork {
    pub(in super::super) products: usize,
    pub(in super::super) bounded: bool,
    pub(in super::super) rows_examined: usize,
    pub(in super::super) vectors_loaded: usize,
}

fn scope_sql(vector_index: &str) -> String {
    format!("FROM rep_search_visible visible
        CROSS JOIN rep_search_entries e ON e.id=visible.entry
        CROSS JOIN rep_search_passages p INDEXED BY rep_search_passages_entry ON p.entry=e.id
        CROSS JOIN rep_search_vectors v {vector_index} ON v.passage=p.id AND v.model=?2
        WHERE visible.head=?1
          AND (?3 IS NULL OR e.kind=?3) AND (?4 IS NULL OR e.subject=?4) AND (?5 IS NULL OR e.scope=?5)")
}

const COVERING_VECTORS: &str = "INDEXED BY rep_search_vector_keys";
const SAMPLE_BATCH: usize = 128;

struct Scope<'a> {
    db: &'a Connection,
    head: &'a str,
    options: SearchOptions<'a>,
    model: &'a str,
    positions: Option<(i64, i64)>,
    media_allowed: &'a mut dyn FnMut(Id) -> bool,
}

type Match = (Id, i64, i64, f32);

#[derive(Clone, Copy)]
struct Coarse {
    similarity: f32,
    order: usize,
    passage: i64,
    part: i64,
}

impl Ord for Coarse {
    fn cmp(&self, other: &Self) -> Ordering {
        self.similarity
            .total_cmp(&other.similarity)
            .then_with(|| other.order.cmp(&self.order))
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
    let mut scope = Scope {
        db,
        head,
        options,
        model: semantic.model,
        positions: if options.kind.is_none() && options.subject.is_none() && options.scope.is_none()
        {
            db.prepare_cached("SELECT id,count FROM rep_search_samples WHERE head=? AND model=? AND complete AND NOT invalid")?
                .query_row(params![head,semantic.model],|r|Ok((r.get(0)?,r.get(1)?))).optional()?
        } else {
            None
        },
        media_allowed: semantic.media_allowed,
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
        let scored = scope.visit(dimensions, count, count, work, &mut exact)?;
        work.products = scored * exact_cost;
    } else {
        let coarse_budget = (budget - exact_cost) / 2;
        let width = (coarse_budget / query_count / count).clamp(1, dimensions.min(128));
        let samples = count.min(coarse_budget / (query_count * width));
        if samples == 0 {
            // Tiny caller-supplied budgets can only afford sampled exact comparisons.
            let samples = count.min(budget / exact_cost);
            let scored = scope.visit(dimensions, count, samples, work, &mut exact)?;
            work.products = scored * exact_cost;
        } else {
            let coarse_products = samples * query_count * width;
            let finalists = samples.min((budget - coarse_products) / exact_cost);
            let shortlist =
                shortlist(&mut scope, &queries, width, count, samples, finalists, work)?;
            let mut statement = db.prepare_cached(
                "SELECT e.object,p.id,p.location,v.part,v.vector FROM rep_search_vectors v
                 JOIN rep_search_passages p ON p.id=v.passage JOIN rep_search_entries e ON e.id=p.entry
                 WHERE v.model=? AND v.passage=? AND v.part=?",
            )?;
            for item in &shortlist {
                let mut rows = statement.query(params![semantic.model, item.passage, item.part])?;
                if let Some(row) = rows.next()? {
                    exact(row)?;
                    work.vectors_loaded += 1;
                }
            }
            work.products += shortlist.len() * exact_cost;
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
    scope: &mut Scope<'_>,
    queries: &[&[f32]],
    width: usize,
    count: usize,
    samples: usize,
    finalists: usize,
    work: &mut VectorWork,
) -> Result<Vec<Coarse>> {
    let prefixes: Vec<_> = queries
        .iter()
        .map(|query| unit_prefix(query, width))
        .collect();
    let query_refs: Vec<_> = prefixes.iter().map(Vec::as_slice).collect();
    let mut decoded = Vec::with_capacity(width);
    let mut best = BinaryHeap::with_capacity(finalists);
    let mut order = 0;
    let scored = scope.visit(width, count, samples, work, |row| {
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
            order,
            passage: row.get(1)?,
            part: row.get(3)?,
        };
        order += 1;
        if best.len() < finalists {
            best.push(Reverse(item));
        } else if best.peek().is_some_and(|worst| worst.0 < item) {
            *best.peek_mut().expect("shortlist is full") = Reverse(item);
        }
        Ok(())
    })?;
    work.products = scored * queries.len() * width;
    Ok(best.into_iter().map(|item| item.0).collect())
}

impl Scope<'_> {
    fn count(&self) -> Result<usize> {
        if let Some((_, count)) = self.positions {
            return Ok(usize::try_from(count)?);
        }
        let sql = format!("SELECT count(*) {}", scope_sql(COVERING_VECTORS));
        let count: i64 = self.db.prepare_cached(&sql)?.query_row(
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
        &mut self,
        width: usize,
        count: usize,
        samples: usize,
        work: &mut VectorWork,
        mut score: impl FnMut(&Row<'_>) -> Result<()>,
    ) -> Result<usize> {
        if samples == 0 {
            return Ok(0);
        }
        let mut scored = 0;
        let media_allowed = &mut self.media_allowed;
        let mut visit = |row: &Row<'_>| {
            // Media vectors may have been prepared before scoped access was enforced.
            if !row.get::<_, bool>(5)? || media_allowed(row.get::<_, String>(0)?.parse()?) {
                score(row)?;
                scored += 1;
            }
            Ok(())
        };
        // Sparse scans read ordered keys first. Materialize embedding BLOBs
        // only for sampled rows, even when the eligible inventory is large.
        let sparse = samples < count;
        let width = (width * 4) as i64;
        if sparse && let Some((sample, _)) = self.positions {
            let mut lookup=self.db.prepare_cached("SELECT e.object,p.id,p.location,v.part,substr(v.vector,1,?4),p.text=''
                FROM rep_search_positions s CROSS JOIN rep_search_vectors v ON v.model=?3 AND v.passage=s.passage AND v.part=s.part
                CROSS JOIN rep_search_passages p ON p.id=v.passage CROSS JOIN rep_search_entries e ON e.id=p.entry
                WHERE s.sample=?1 AND s.ordinal=?2")?;
            for visited in 0..samples {
                let ordinal = midpoint(visited, count, samples) as i64;
                let mut rows = lookup.query(params![sample, ordinal, self.model, width])?;
                let row = rows
                    .next()?
                    .ok_or_else(|| anyhow::anyhow!("missing prepared sample position"))?;
                visit(row)?;
                work.rows_examined += 1;
                work.vectors_loaded += 1;
            }
            return Ok(scored);
        }
        let (columns, index) = if sparse {
            ("p.id,v.part", COVERING_VECTORS)
        } else {
            (
                "e.object,p.id,p.location,v.part,substr(v.vector,1,?6),p.text=''",
                "",
            )
        };
        let sql = format!(
            "SELECT {columns} {} ORDER BY visible.object,visible.section,p.location,v.part",
            scope_sql(index)
        );
        let mut statement = self.db.prepare_cached(&sql)?;
        let kind = self.options.kind.map(|kind| kind as i64);
        let bound: [&dyn rusqlite::ToSql; 6] = [
            &self.head,
            &self.model,
            &kind,
            &self.options.subject,
            &self.options.scope,
            &width,
        ];
        let mut rows = statement.query(&bound[..if sparse { 5 } else { 6 }])?;
        let mut vectors = self.db.prepare_cached(
            "SELECT e.object,p.id,p.location,v.part,substr(v.vector,1,?3),p.text=''
             FROM json_each(?2) sample
             CROSS JOIN rep_search_vectors v ON v.model=?1
                AND v.passage=json_extract(sample.value,'$[0]')
                AND v.part=json_extract(sample.value,'$[1]')
             CROSS JOIN rep_search_passages p ON p.id=v.passage
             CROSS JOIN rep_search_entries e ON e.id=p.entry ORDER BY sample.key",
        )?;
        let mut keys = Vec::with_capacity(SAMPLE_BATCH);
        let mut available = 0;
        let mut visited = 0;
        while let Some(row) = rows.next()? {
            work.rows_examined += 1;
            if available == midpoint(visited, count, samples) {
                if sparse {
                    keys.push((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?));
                    if keys.len() == SAMPLE_BATCH {
                        work.vectors_loaded +=
                            visit_samples(&mut vectors, self.model, width, &keys, &mut visit)?;
                        keys.clear();
                    }
                } else {
                    visit(row)?;
                    work.vectors_loaded += 1;
                }
                visited += 1;
                if visited == samples {
                    break;
                }
            }
            available += 1;
        }
        if !keys.is_empty() {
            work.vectors_loaded +=
                visit_samples(&mut vectors, self.model, width, &keys, &mut visit)?;
        }
        Ok(scored)
    }
}

fn midpoint(index: usize, count: usize, samples: usize) -> usize {
    (((2 * index as u128 + 1) * count as u128) / (2 * samples as u128)) as usize
}

fn visit_samples(
    vectors: &mut Statement<'_>,
    model: &str,
    width: i64,
    keys: &[(i64, i64)],
    mut visit: impl FnMut(&Row<'_>) -> Result<()>,
) -> Result<usize> {
    let mut rows = vectors.query(params![model, serde_json::to_string(keys)?, width])?;
    let mut loaded = 0;
    while let Some(row) = rows.next()? {
        visit(row)?;
        loaded += 1;
    }
    ensure!(loaded == keys.len(), "sampled vector disappeared");
    Ok(loaded)
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
