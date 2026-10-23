//! Loads a JSON Lines file, builds the index and runs queries.
//!
//! Usage: search FILE.jsonl [QUERY ...]
//! With no queries on the command line, reads one query per line from stdin.

use engine::index::{Index, Settings};
use engine::search::{intersect, plan};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::time::Instant;

const SHOWN: usize = 5;
const RUNS: usize = 21;

fn run(index: &Index, query: &str) {
    let terms = plan(index, query);
    let docs = intersect(index, &terms);
    // A single run is noisy (cold caches), so report the median of several.
    let mut times: Vec<f64> = (0..RUNS)
        .map(|_| {
            let start = Instant::now();
            std::hint::black_box(intersect(index, &plan(index, query)));
            start.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    times.sort_by(f64::total_cmp);
    println!("\n\"{query}\": {} hits in {:.3} ms (median of {RUNS})", docs.len(), times[RUNS / 2]);
    for term in &terms {
        let shown: Vec<String> =
            term.matches.iter().take(4).map(|m| format!("{} ({} typo)", m.word, m.typos)).collect();
        let more = term.matches.len().saturating_sub(shown.len());
        let more = if more > 0 { format!(" +{more} more") } else { String::new() };
        println!("  {:>12} -> {}{more}", term.text, shown.join(", "));
    }
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
