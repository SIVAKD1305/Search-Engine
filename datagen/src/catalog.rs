//! A large, realistic product catalog for load and scale testing.
//!
//! Unlike the small sample datasets, the vocabulary is big: tens of
//! thousands of invented words, thousands of brands, hundreds of category
//! paths and model numbers like "ZX-4821". Description words follow a Zipf
//! distribution (a few words are very common, most are rare), as in real
//! text.
//!
//! Next to the records it writes a query set sampled from them, with the
//! mix users type: one to three words, half-typed last words, typos, and
//! some filters.

use rand::Rng;
use rand::SeedableRng;
use rand::seq::SliceRandom;
use rand_chacha::ChaCha8Rng;
use serde_json::{Value, json};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

const VOCABULARY: usize = 40_000;
const BRANDS: usize = 2_000;
const NOUNS: usize = 3_000;
const QUERIES: usize = 5_000;

const ONSETS: &[&str] = &[
    "b", "br", "c", "ch", "cl", "d", "dr", "f", "fl", "g", "gr", "h", "j", "k", "kr", "l", "m", "n", "p",
    "pl", "pr", "r", "s", "sh", "sk", "st", "t", "tr", "v", "w", "z",
];
const VOWELS: &[&str] = &["a", "e", "i", "o", "u", "ai", "ea", "io", "ou", "y"];
const CODAS: &[&str] = &["", "", "", "n", "r", "s", "l", "m", "x", "t", "nd", "rk", "st"];

const COLORS: &[&str] = &[
    "Black", "White", "Silver", "Blue", "Red", "Green", "Grey", "Rose Gold", "Navy", "Beige", "Orange", "Purple",
];
const DEPARTMENTS: &[&str] = &[
    "Electronics", "Home", "Kitchen", "Garden", "Sports", "Fashion", "Toys", "Beauty", "Office", "Automotive",
    "Books", "Music", "Health", "Pets", "Baby", "Tools", "Outdoor", "Gaming", "Travel", "Crafts",
];

/// Random syllables can spell offensive words; those are skipped.
const BLOCKED: &[&str] = &[
    "shit", "fuck", "cunt", "cock", "dick", "piss", "slut", "whore", "nig", "fag", "rape", "porn", "sex", "tit",
    "anal", "nazi", "kill",
];

/// An invented, pronounceable word of 1 to 4 syllables.
fn invented_word(rng: &mut ChaCha8Rng) -> String {
    loop {
        let syllables = rng.gen_range(1..=4);
        let mut word = String::new();
        for _ in 0..syllables {
            word.push_str(ONSETS.choose(rng).unwrap());
            word.push_str(VOWELS.choose(rng).unwrap());
        }
        word.push_str(CODAS.choose(rng).unwrap());
        if !BLOCKED.iter().any(|b| word.contains(b)) {
            return word;
        }
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// Distinct invented words.
fn word_list(rng: &mut ChaCha8Rng, n: usize, min_len: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut words = Vec::with_capacity(n);
    while words.len() < n {
        let w = invented_word(rng);
        if w.len() >= min_len && seen.insert(w.clone()) {
            words.push(w);
        }
    }
    words
}

/// Samples ranks 0..n with probability proportional to 1 / (rank + 1)^s.
struct Zipf {
    cumulative: Vec<f64>,
}

impl Zipf {
    fn new(n: usize, s: f64) -> Self {
        let mut total = 0.0;
        let cumulative = (0..n)
            .map(|r| {
                total += 1.0 / ((r + 1) as f64).powf(s);
                total
            })
            .collect();
        Zipf { cumulative }
    }

    fn sample(&self, rng: &mut ChaCha8Rng) -> usize {
        let x = rng.gen_range(0.0..*self.cumulative.last().unwrap());
        self.cumulative.partition_point(|&c| c < x)
    }
}

struct Catalog {
    vocabulary: Vec<String>,
    zipf: Zipf,
    brands: Vec<String>,
    nouns: Vec<String>,
    adjectives: Vec<String>,
    categories: Vec<String>,
}

impl Catalog {
    fn new(rng: &mut ChaCha8Rng) -> Self {
        let vocabulary = word_list(rng, VOCABULARY, 3);
        let brands = word_list(rng, BRANDS, 4).iter().map(|w| capitalize(w)).collect();
        let nouns = word_list(rng, NOUNS, 4).iter().map(|w| capitalize(w)).collect();
        let adjectives = word_list(rng, 400, 4).iter().map(|w| capitalize(w)).collect();
        let mut categories = Vec::new();
        for department in DEPARTMENTS {
            for _ in 0..15 {
                let middle = capitalize(&invented_word(rng));
                let leaf = capitalize(&invented_word(rng));
                categories.push(format!("{department} > {middle} > {leaf}"));
            }
        }
        Catalog { vocabulary, zipf: Zipf::new(VOCABULARY, 1.07), brands, nouns, adjectives, categories }
    }

    fn product(&self, rng: &mut ChaCha8Rng, id: usize) -> Value {
        // Popular brands and nouns appear more often, as in real catalogs.
        let brand = &self.brands[rng.gen_range(0..BRANDS).min(rng.gen_range(0..BRANDS))];
        let noun = &self.nouns[rng.gen_range(0..NOUNS).min(rng.gen_range(0..NOUNS))];
        let adjective = self.adjectives.choose(rng).unwrap();
        let letters: String = (0..2).map(|_| rng.gen_range(b'A'..=b'Z') as char).collect();
        let model = format!("{letters}-{}", rng.gen_range(100..10_000));
        let words: Vec<&str> = (0..rng.gen_range(15..40)).map(|_| self.vocabulary[self.zipf.sample(rng)].as_str()).collect();
        let mut tags: Vec<&str> = (0..rng.gen_range(1..5)).map(|_| self.vocabulary[self.zipf.sample(rng)].as_str()).collect();
        tags.sort_unstable();
        tags.dedup();
        json!({
            "objectID": format!("c{id}"),
            "name": format!("{brand} {adjective} {noun} {model}"),
            "brand": brand,
            "category": self.categories.choose(rng).unwrap(),
            "color": COLORS.choose(rng).unwrap(),
            "model": model,
            "tags": tags,
            "description": words.join(" "),
            "price": (rng.gen_range(1.0..2000.0_f64) * 100.0).round() / 100.0,
            "rating": (rng.gen_range(1.0..5.0_f64) * 100.0).round() / 100.0,
            "reviews_count": rng.gen_range(0..50_000u32),
            "in_stock": rng.gen_bool(0.85),
        })
    }
}

/// Replaces, drops, inserts or swaps one letter.
fn with_typo(rng: &mut ChaCha8Rng, word: &str) -> String {
    let mut chars: Vec<char> = word.chars().collect();
    let i = rng.gen_range(1..chars.len());
    match rng.gen_range(0..4) {
        0 => chars[i] = (b'a' + rng.gen_range(0..26)) as char,
        1 => {
            chars.remove(i);
        }
        2 => chars.insert(i, (b'a' + rng.gen_range(0..26)) as char),
        _ => chars.swap(i - 1, i),
    }
    chars.into_iter().collect()
}

/// A query a user might type to find (or browse near) `record`.
fn query_for(rng: &mut ChaCha8Rng, record: &Value) -> Value {
    let name: Vec<String> = record["name"].as_str().unwrap().split(' ').map(str::to_lowercase).collect();
    let description: Vec<&str> = record["description"].as_str().unwrap().split(' ').collect();
    let count = rng.gen_range(1..=3).min(name.len());
    let mut words: Vec<String> = match rng.gen_range(0..10) {
        0..=4 => name.choose_multiple(rng, count).cloned().collect(),
        5..=7 => {
            let mut w = vec![name[0].clone()];
            w.push(description.choose(rng).unwrap().to_string());
            w
        }
        _ => vec![description.choose(rng).unwrap().to_string()],
    };
    if rng.gen_bool(0.2)
        && let Some(w) = words.iter_mut().find(|w| w.len() >= 5)
    {
        *w = with_typo(rng, w);
    }
    if rng.gen_bool(0.4) {
        // Still typing the last word.
        let last = words.last_mut().unwrap();
        let keep = rng.gen_range(1..=last.chars().count());
        *last = last.chars().take(keep).collect();
    }
    let mut query = json!({ "query": words.join(" ") });
    match rng.gen_range(0..10) {
        0 => query["filters"] = json!(format!("price < {}", rng.gen_range(20..500))),
        1 => query["filters"] = json!(format!("in_stock:true AND rating >= {}", rng.gen_range(2..5))),
        2 => query["facets"] = json!(["brand", "category"]),
        _ => {}
    }
    query
}

pub fn write_catalog(out_dir: &Path, records: usize, seed: u64) -> std::io::Result<()> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let catalog = Catalog::new(&mut rng);
    let path = out_dir.join("catalog.jsonl");
    let mut out = BufWriter::new(File::create(&path)?);
    // Keep an evenly spread sample of records to draw queries from.
    let every = (records / QUERIES).max(1);
    let mut sample = Vec::new();
    for id in 0..records {
        let record = catalog.product(&mut rng, id);
        serde_json::to_writer(&mut out, &record)?;
        out.write_all(b"\n")?;
        if id % every == 0 && sample.len() < QUERIES {
            sample.push(record);
        }
    }
    out.flush()?;
    println!("wrote {records:>7} records to {}", path.display());

    let path = out_dir.join("catalog-queries.jsonl");
    let mut out = BufWriter::new(File::create(&path)?);
    for record in &sample {
        serde_json::to_writer(&mut out, &query_for(&mut rng, record))?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    println!("wrote {:>7} queries to {}", sample.len(), path.display());
    Ok(())
}
