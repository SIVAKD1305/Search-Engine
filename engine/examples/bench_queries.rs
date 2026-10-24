//! Loads a catalog and runs a query set in-process, reporting indexing time,
//! memory, latency percentiles and the slowest queries.
//!
//! Usage (from the repository root):
//! `cargo run --release -p engine --example bench_queries -- RECORDS.jsonl SETTINGS.json QUERIES.jsonl`
//!
//! Each line of QUERIES.jsonl holds search parameters, e.g.
//! `{"query": "wireless he", "filters": "price < 100"}`.

use engine::index::{Index, Settings};
use engine::search::{SearchParams, search};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::time::Instant;

/// (current, peak) resident memory in MB, from /proc on Linux.
fn memory_mb() -> (f64, f64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|kb| kb.parse::<f64>().ok())
            .map_or(f64::NAN, |kb| kb / 1024.0)
    };
    (field("VmRSS:"), field("VmHWM:"))
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [records, settings, queries] = args.as_slice() else {
        eprintln!("usage: bench_queries RECORDS.jsonl SETTINGS.json QUERIES.jsonl");
        std::process::exit(2);
    };
    let settings: Settings = serde_json::from_reader(File::open(settings).unwrap()).unwrap();
    let start = Instant::now();
    let mut index = Index::new(settings);
    index.load_jsonl(BufReader::new(File::open(records).unwrap())).unwrap();
    let stats = index.stats();
    let (rss, peak) = memory_mb();
    println!(
        "indexed {} records in {:.1} s: {} words, {} prefixes, {} hits; memory {:.0} MB (peak {:.0} MB)",
        stats.records,
        start.elapsed().as_secs_f64(),
        stats.words,
        stats.prefixes,
        stats.hits,
        rss,
        peak
    );

    let params: Vec<SearchParams> = BufReader::new(File::open(queries).unwrap())
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect();
    // One warm-up pass, then a measured pass.
    for p in &params {
        let _ = search(&index, p);
    }
    let mut timed: Vec<(f64, &SearchParams, u64)> = params
        .iter()
        .map(|p| {
            let start = Instant::now();
            let hits = search(&index, p).map(|r| r.nb_hits).unwrap_or(0);
            (start.elapsed().as_secs_f64() * 1000.0, p, hits)
        })
        .collect();
    let mut times: Vec<f64> = timed.iter().map(|t| t.0).collect();
    times.sort_by(f64::total_cmp);
    let total: f64 = times.iter().sum();
    println!(
        "{} queries: mean {:.2} ms, p50 {:.2}, p90 {:.2}, p99 {:.2}, max {:.2} ms; {:.0} queries/s on one core",
        times.len(),
        total / times.len() as f64,
        percentile(&times, 0.5),
        percentile(&times, 0.9),
        percentile(&times, 0.99),
        times[times.len() - 1],
        times.len() as f64 / (total / 1000.0)
    );
    timed.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("slowest:");
    for (ms, p, hits) in timed.iter().take(10) {
        println!("  {ms:>8.2} ms  {hits:>7} hits  {:?}  filters={:?}", p.query, p.filters);
    }
}
