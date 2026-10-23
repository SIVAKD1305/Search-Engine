//! Loads a JSON Lines file, builds the index and runs queries.
//!
//! Usage: search FILE.jsonl [--settings SETTINGS.json] [QUERY ...]
//! With no queries on the command line, reads one query per line from stdin.

use engine::index::{Index, Settings};
use engine::search::search;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::time::Instant;

const SHOWN: usize = 5;
const RUNS: usize = 21;

fn label(index: &Index, doc: u32) -> String {
    let record = index.store().get(doc).expect("hit is live");
    let text = ["name", "title"].iter().find_map(|k| record.get(*k).and_then(|v| v.as_str())).unwrap_or("");
    format!("{:>6}  {text}", record["objectID"].as_str().unwrap_or(""))
}

fn run(index: &Index, query: &str) {
    let results = search(index, query, 0, SHOWN);
    // A single run is noisy (cold caches), so report the median of several.
    let mut times: Vec<f64> = (0..RUNS)
        .map(|_| {
            let start = Instant::now();
            std::hint::black_box(search(index, query, 0, SHOWN));
            start.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    times.sort_by(f64::total_cmp);
    println!("\n\"{query}\": {} hits in {:.3} ms (median of {RUNS})", results.nb_hits, times[RUNS / 2]);
    for term in &results.terms {
        let shown: Vec<String> =
            term.matches.iter().take(4).map(|m| format!("{} ({} typo)", m.word, m.typos)).collect();
        let more = term.matches.len().saturating_sub(shown.len());
        let more = if more > 0 { format!(" +{more} more") } else { String::new() };
        println!("  {:>12} -> {}{more}", term.text, shown.join(", "));
    }
    println!("  {:<44} typo words prox attribute exact custom", "");
    for hit in &results.hits {
        let i = &hit.info;
        let attr = i.first_matched_attribute.map(|a| index.attributes()[a as usize].as_str()).unwrap_or("-");
        let custom: Vec<String> = i.custom.iter().map(|v| v.map_or("-".into(), |v| v.to_string())).collect();
        println!(
            "  {:<44} {:>4} {:>5} {:>4} {:<9.9} {:>5} {}",
            label(index, hit.doc).chars().take(44).collect::<String>(),
            i.nb_typos,
            i.words,
            i.proximity_distance,
            attr,
            i.nb_exact_words,
            custom.join(",")
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut path = None;
    let mut settings = Settings::default();
    let mut queries = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--settings" => {
                let file = args.next().ok_or("--settings needs a file")?;
                settings = serde_json::from_reader(BufReader::new(File::open(&file)?))
                    .map_err(|e| format!("{file}: {e}"))?;
            }
            _ if path.is_none() => path = Some(arg),
            _ => queries.push(arg),
        }
    }
    let path = path.ok_or("usage: search FILE.jsonl [--settings SETTINGS.json] [QUERY ...]")?;

    let start = Instant::now();
    let mut index = Index::new(settings);
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
