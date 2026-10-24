//! Times the stages of individual queries on any dataset.
//!
//! Usage: `cargo run --release -p engine --example profile_query -- RECORDS.jsonl SETTINGS.json QUERY...`

use engine::bucket::rank_page;
use engine::index::{Index, Settings};
use engine::search::{SearchParams, matching, plan, search};
use std::fs::File;
use std::io::BufReader;
use std::time::Instant;

fn time<T>(runs: u32, mut f: impl FnMut() -> T) -> f64 {
    let start = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(f());
    }
    start.elapsed().as_secs_f64() * 1000.0 / runs as f64
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (records, settings, queries) = (&args[0], &args[1], &args[2..]);
    let settings: Settings = serde_json::from_reader(File::open(settings).unwrap()).unwrap();
    let mut index = Index::new(settings);
    index.load_jsonl(BufReader::new(File::open(records).unwrap())).unwrap();
    for q in queries {
        let terms = plan(&index, q);
        let candidates = matching(&index, &terms);
        let words: usize = terms.iter().map(|t| t.matches.len()).sum();
        let no_highlight = SearchParams { attributes_to_highlight: Some(vec![]), ..SearchParams::new(q.as_str()) };
        println!(
            "{q:>22}: {} hits, {words} matched words | plan {:.2}  match {:.2}  rank {:.2}  search w/o highlight {:.2}  full search {:.2} ms",
            candidates.len(),
            time(10, || plan(&index, q)),
            time(10, || matching(&index, &terms)),
            time(10, || rank_page(&index, &terms, &candidates, &[], 0, 20)),
            time(10, || search(&index, &no_highlight).unwrap()),
            time(10, || search(&index, &SearchParams::new(q.as_str())).unwrap()),
        );
    }
}
