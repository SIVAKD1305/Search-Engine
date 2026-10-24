//! Times each stage of a search on the sample products.
//!
//! Run from the repository root after `cargo run --release -p datagen -- data`:
//! `cargo run --release -p engine --example profile_rank`

use engine::index::{Index, Settings};
use engine::bucket::rank_page;
use engine::ranking::rank;
use engine::search::{SearchParams, matching, plan, search};
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
    let settings: Settings = serde_json::from_reader(std::fs::File::open("config/products.json").unwrap()).unwrap();
    let mut index = Index::new(settings);
    index.load_jsonl(BufReader::new(std::fs::File::open("data/products.jsonl").unwrap())).unwrap();
    for q in ["s", "", "h", "wireless", "wireless head"] {
        let terms = plan(&index, q);
        let cands = matching(&index, &terms);
        println!(
            "{q:>14}: plan {:.3}  match {:.3}  full rank {:.3}  page rank {:.3}  search {:.3} ms  ({} hits)",
            time(50, || plan(&index, q)),
            time(50, || matching(&index, &terms)),
            time(50, || rank(&index, &terms, &cands, &[])),
            time(50, || rank_page(&index, &terms, &cands, &[], 0, 20)),
            time(50, || search(&index, &SearchParams::new(q)).unwrap()),
            cands.len()
        );
    }
}
