#![cfg(feature = "semantic-search")]

use anyhow::{Result, ensure};
use naome_protocol::{
    profile::{Document, Kind, PUBLIC},
    search::{EmbeddingGemma, SearchMode, SearchOptions},
    store::Archive,
    wire::{Format, Id},
};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
struct Case {
    language: String,
    topic: String,
    passage: String,
    same: String,
    cross: String,
    cross_topic: String,
}

#[derive(Default)]
struct Score {
    found: usize,
    ndcg: f64,
}

impl Score {
    fn add(&mut self, rank: Option<usize>) {
        if let Some(rank) = rank {
            self.found += 1;
            self.ndcg += 1.0 / (rank as f64 + 1.0).log2();
        }
    }
}

fn p95(times: &mut [Duration]) -> Duration {
    times.sort_unstable();
    times[(times.len() * 95).div_ceil(100).saturating_sub(1)]
}

#[test]
#[ignore = "requires local EmbeddingGemma 2 weights and Python inference dependencies"]
fn archive_retrieval_in_both_language_directions() -> Result<()> {
    let cases: Vec<Case> = include_str!("fixtures/search-multilingual.jsonl")
        .lines()
        .map(serde_json::from_str)
        .collect::<serde_json::Result<_>>()?;
    let directory =
        PathBuf::from(std::env::var_os("NAOME_SEARCH_MODEL").expect("NAOME_SEARCH_MODEL"));
    let python =
        PathBuf::from(std::env::var_os("NAOME_SEARCH_PYTHON").unwrap_or_else(|| "python3".into()));
    let start = Instant::now();
    let mut model = EmbeddingGemma::open(&directory, &python)?;
    let model_open = start.elapsed();
    let archive = Archive::memory()?;
    let mut sources = BTreeMap::new();
    for (i, case) in cases.iter().enumerate() {
        let format = if i % 2 == 0 { Format::A } else { Format::B };
        let id = archive.capture(
            case.passage.as_bytes(),
            "text/plain",
            128,
            Id::blob(PUBLIC),
            format,
        )?;
        sources.insert(&case.topic, (&case.language, id));
    }
    for (subject, text) in [
        (
            "Fishing fleet statistics",
            "A fleet of twenty recreational craft sailed on weekends.",
        ),
        (
            "Road traffic estimates",
            "The team recorded the number of cars per hour near the bridge, with no particle measurement.",
        ),
        (
            "Küstenwetter",
            "Am Morgen wurde Regen gemeldet. Die ungeschützte Fläche wurde für Bodenproben gesperrt.",
        ),
        (
            "陶器の模様",
            "考古学者は形や模様だけを分類した。焼成条件は分かっていない。",
        ),
        (
            "Calibración de una cámara",
            "Se ajustó una cámara de laboratorio con una cuadrícula. No se utilizó ningún avión.",
        ),
    ] {
        let document = Document::new(
            Kind::Claim,
            subject,
            "distractor",
            json!({"text": text}),
            vec![],
        )?;
        archive.document(&document, Format::B, Id::blob(PUBLIC))?;
    }
    let snapshot = archive.snapshot("multilingual search trial", Default::default())?;
    let hybrid = SearchOptions {
        mode: SearchMode::Hybrid,
        ..Default::default()
    };
    let start = Instant::now();
    let warmup = archive.find(&snapshot, &cases[0].same, hybrid, Some(&mut model))?;
    let cold_index = start.elapsed();
    ensure!(warmup.index_complete && warmup.semantic_index_complete == Some(true));
    let mut results = [
        [Score::default(), Score::default()],
        [Score::default(), Score::default()],
        [Score::default(), Score::default()],
    ];
    let mut times = [Vec::new(), Vec::new(), Vec::new()];
    let mut coverage: BTreeMap<&String, [bool; 3]> = cases
        .iter()
        .map(|case| (&case.language, [false; 3]))
        .collect();
    for case in &cases {
        for (set, query, topic) in [
            (0, &case.same, &case.topic),
            (1, &case.cross, &case.cross_topic),
        ] {
            let &(source_language, expected) = sources.get(topic).expect("known topic");
            let mut ranks = [None; 3];
            for (i, mode) in [
                SearchMode::Lexical,
                SearchMode::Semantic,
                SearchMode::Hybrid,
            ]
            .into_iter()
            .enumerate()
            {
                let start = Instant::now();
                let report = archive.find(
                    &snapshot,
                    query,
                    SearchOptions { mode, ..hybrid },
                    Some(&mut model),
                )?;
                times[i].push(start.elapsed());
                let rank = report
                    .hits
                    .iter()
                    .position(|hit| hit.id == expected)
                    .map(|rank| rank + 1);
                results[i][set].add(rank);
                ranks[i] = rank;
            }
            if set == 0 {
                coverage.get_mut(&case.language).unwrap()[0] =
                    ranks[2].is_some_and(|rank| rank <= 3);
            } else {
                let passed = ranks[2].is_some_and(|rank| rank <= 3);
                coverage.get_mut(&case.language).unwrap()[1] = passed;
                coverage.get_mut(source_language).unwrap()[2] = passed;
            }
            println!(
                "{} -> {} {} {ranks:?}",
                case.language, source_language, topic
            );
        }
    }
    let supported = coverage
        .values()
        .filter(|checks| checks.iter().all(|&passed| passed))
        .count();
    println!(
        "model {:?}; open {model_open:?}; cold index + first query {cold_index:?}",
        warmup.model
    );
    let count = cases.len() as f64;
    for (i, (scores, times)) in results.iter().zip(&mut times).enumerate() {
        println!(
            "mode {i} (lexical/semantic/hybrid), warm p95 {:?}",
            p95(times)
        );
        for (set, score) in scores.iter().enumerate() {
            println!(
                "set {set} (same/cross) Recall@10 {:.3} nDCG@10 {:.3}",
                score.found as f64 / count,
                score.ndcg / count
            );
        }
    }
    println!(
        "Top-three in all three roles: {supported}/{} languages",
        cases.len()
    );
    println!(
        "misses: {:?}",
        coverage
            .iter()
            .filter(|(_, checks)| checks.iter().any(|&passed| !passed))
            .collect::<Vec<_>>()
    );
    let negatives = [
        "marzipan ukulele chords",
        "qui a gagné la finale du championnat de football",
        "电影票退订政策是什么",
        "طريقة خبز كعكة الشوكولاتة",
        "क्या किराए पर कार मिलती है",
        "правила игры в шахматы",
        "wie kann ich meinen Reisepass verlängern?",
    ];
    let mut empty = 0;
    for query in negatives {
        let report = archive.find(&snapshot, query, hybrid, Some(&mut model))?;
        empty += usize::from(report.hits.is_empty());
        println!(
            "unmatched query: {query}, {} hits, first {:?}",
            report.hits.len(),
            report.hits.first().map(|hit| (
                &hit.snippet,
                hit.semantic_similarity,
                hit.lexical_rank
            ))
        );
    }
    println!(
        "Empty results for {empty}/{} unmatched queries",
        negatives.len()
    );
    ensure!(
        empty >= 5,
        "unmatched queries should generally return an empty result"
    );
    ensure!(
        results[2][1].ndcg > results[0][1].ndcg,
        "hybrid must improve cross-language relevance"
    );
    ensure!(
        supported * 10 >= cases.len() * 9,
        "hybrid must cover 90% of the trial language set"
    );
    Ok(())
}
