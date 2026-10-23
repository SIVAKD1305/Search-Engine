//! Loads a JSON Lines file, builds the index and runs queries.
//!
//! Usage: search FILE.jsonl [QUERY ...]
//! With no queries on the command line, reads one query per line from stdin.

use engine::index::{Index, Settings};
use engine::search::candidates;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::time::Instant;

const SHOWN: usize = 5;

fn run(index: &Index, query: &str) {
    let start = Instant::now();
    let docs = candidates(index, query);
    let elapsed = start.elapsed();
    println!("\n\"{query}\": {} hits in {:.3} ms", docs.len(), elapsed.as_secs_f64() * 1000.0);
    for id in docs.iter().take(SHOWN) {
        let doc = index.store().get(id).expect("candidate is live");
        let label = ["name", "title"]
            .iter()
            .find_map(|k| doc.get(*k).and_then(|v| v.as_str()))
            .unwrap_or("");
        println!("  {:>7}  {label}", doc["objectID"].as_str().unwrap_or(""));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: search FILE.jsonl [QUERY ...]")?;

    let start = Instant::now();
    let mut index = Index::new(Settings::default());
    index.load_jsonl(BufReader::new(File::open(&path)?))?;
    let stats = index.stats();
    println!(
        "indexed {} records in {:.0} ms: {} words, {} prefixes, {} hits",
        stats.records,
        start.elapsed().as_secs_f64() * 1000.0,
        stats.words,
        stats.prefixes,
        stats.hits
    );
    println!("searchable attributes: {}", index.attributes().join(", "));

    let queries: Vec<String> = args.collect();
    if queries.is_empty() {
        for line in io::stdin().lock().lines() {
            run(&index, line?.trim());
        }
    } else {
        for query in &queries {
            run(&index, query);
        }
    }
    Ok(())
}
