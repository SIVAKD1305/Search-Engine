//! Indexes the first N records of a file, for profiling indexing.
//!
//! Usage: `cargo run --release -p engine --example profile_index -- RECORDS.jsonl SETTINGS.json N`

use engine::index::{Index, Settings};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let settings: Settings = serde_json::from_reader(File::open(&args[1]).unwrap()).unwrap();
    let limit: usize = args[2].parse().unwrap();
    let lines: Vec<String> = BufReader::new(File::open(&args[0]).unwrap()).lines().take(limit).map(Result::unwrap).collect();
    let start = Instant::now();
    let records: Vec<serde_json::Value> = lines.iter().map(|l| serde_json::from_str(l).unwrap()).collect();
    let parsed = start.elapsed();
    let mut index = Index::new(settings);
    let start = Instant::now();
    for r in records {
        index.add(r).unwrap();
    }
    let indexed = start.elapsed();
    println!(
        "{} records: JSON parsing {:.0} µs/record, indexing {:.0} µs/record",
        index.stats().records,
        parsed.as_secs_f64() * 1e6 / limit as f64,
        indexed.as_secs_f64() * 1e6 / limit as f64
    );
}
