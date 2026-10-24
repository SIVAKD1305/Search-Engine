//! Generates deterministic sample datasets (products, movies, articles) as JSON Lines.
//!
//! Usage: datagen [OUT_DIR] [--products N] [--movies N] [--articles N] [--seed S]
//!        datagen [OUT_DIR] --catalog N [--seed S]
//!
//! `--catalog N` writes only the large catalog (`catalog.jsonl`) and a
//! query set sampled from it (`catalog-queries.jsonl`); see `catalog.rs`.
//!
//! All names are synthetic. The same seed always produces the same files, so
//! relevance tests can rely on exact records.

mod catalog;

use rand::Rng;
use rand::SeedableRng;
use rand::seq::SliceRandom;
use rand_chacha::ChaCha8Rng;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

const BRANDS: &[&str] = &[
    "Nordvik", "Zeltra", "Lumora", "Kestrel", "Brightwell", "Orbix", "Valeo Home", "Tundra",
    "Élan", "Brücke", "Sakura Labs", "Pinecrest", "Novatek", "Aurelia", "Ironfield", "Café Noir",
];

const ADJECTIVES: &[&str] = &[
    "Wireless", "Portable", "Ergonomic", "Compact", "Waterproof", "Smart", "Premium", "Classic",
    "Ultra", "Lightweight", "Stainless", "Organic", "Vintage", "Foldable", "Rechargeable", "Silent",
];

/// (category, nouns that belong to it)
const CATEGORIES: &[(&str, &[&str])] = &[
    ("Electronics > Audio", &["Headphones", "Earbuds", "Speaker", "Soundbar", "Microphone"]),
    ("Electronics > Computers", &["Laptop", "Keyboard", "Mouse", "Monitor", "Webcam"]),
    ("Electronics > Phones", &["Smartphone", "Charger", "Phone Case", "Power Bank"]),
    ("Home > Kitchen", &["Coffee Maker", "Blender", "Kettle", "Toaster", "Knife Set"]),
    ("Home > Furniture", &["Desk", "Office Chair", "Bookshelf", "Lamp", "Sofa"]),
    ("Sports > Outdoor", &["Backpack", "Tent", "Water Bottle", "Hiking Boots", "Sleeping Bag"]),
    ("Sports > Fitness", &["Yoga Mat", "Dumbbells", "Resistance Bands", "Jump Rope"]),
    ("Fashion > Accessories", &["Watch", "Sunglasses", "Wallet", "Belt", "Scarf"]),
];

const COLORS: &[&str] = &["Black", "White", "Silver", "Blue", "Red", "Green", "Grey", "Rose Gold"];

const FEATURES: &[&str] = &[
    "long battery life", "noise cancelling", "fast charging", "bluetooth 5.3", "easy to clean",
    "dishwasher safe", "made from recycled materials", "two year warranty", "adjustable height",
    "built-in microphone", "USB-C", "anti-slip surface", "machine washable", "scratch resistant",
];

const FIRST_NAMES: &[&str] = &[
    "Amelia", "Ravi", "Sofia", "Kenji", "Lucas", "Priya", "Mateo", "Chloé", "Omar", "Ingrid",
    "Thabo", "Mei", "Noah", "Zoë", "Arjun", "Elena", "Jonas", "Fatima", "Diego", "Hana",
];

const LAST_NAMES: &[&str] = &[
    "Hartley", "Sharma", "Moreau", "Tanaka", "Okafor", "Lindqvist", "García", "Novak", "Haddad",
    "Fischer", "Kowalski", "Rossi", "Nguyen", "Brennan", "Petrov", "Mbeki", "Larsen", "Costa",
];

const GENRES: &[&str] = &[
    "Action", "Adventure", "Comedy", "Drama", "Horror", "Science Fiction", "Romance", "Thriller",
    "Animation", "Documentary", "Fantasy", "Mystery",
];

const TITLE_WORDS_A: &[&str] = &[
    "Midnight", "Silent", "Last", "Broken", "Golden", "Hidden", "Crimson", "Frozen", "Electric",
    "Distant", "Wild", "Forgotten", "Iron", "Paper", "Velvet", "Burning",
];

const TITLE_WORDS_B: &[&str] = &[
    "River", "Kingdom", "Empire", "Horizon", "Garden", "Signal", "Shadow", "Harbor", "Machine",
    "Station", "Promise", "Frontier", "Mirror", "Orchard", "Storm", "Café",
];

const PLOT_SUBJECTS: &[&str] = &[
    "a retired detective", "two estranged sisters", "a young astronaut", "a small-town baker",
    "an ambitious lawyer", "a team of thieves", "a lonely robot", "a famous violinist",
];

const PLOT_ACTIONS: &[&str] = &[
    "uncovers a secret that", "must cross the desert before", "falls in love while",
    "races against time as", "returns home when", "fights a corrupt empire after",
];

const PLOT_ENDINGS: &[&str] = &[
    "a storm destroys the harbor.", "the city goes dark.", "an old friend disappears.",
    "the kingdom prepares for war.", "a mysterious signal arrives from space.",
    "the family business is about to close.",
];

const TOPICS: &[(&str, &[&str])] = &[
    ("search", &["inverted index", "relevance", "typo tolerance", "ranking", "tokenization", "query latency"]),
    ("databases", &["transactions", "replication", "indexes", "query planner", "write-ahead log"]),
    ("distributed systems", &["consensus", "Raft", "partitioning", "failover", "clock skew"]),
    ("web performance", &["caching", "CDN", "compression", "latency", "HTTP/2"]),
    ("machine learning", &["embeddings", "vector search", "training data", "evaluation", "overfitting"]),
    ("security", &["API keys", "rate limiting", "HMAC signatures", "access control", "audit logs"]),
];

const SENTENCE_TEMPLATES: &[&str] = &[
    "Understanding {k} is essential when you build reliable {t} systems.",
    "Many teams underestimate how {k} affects the user experience.",
    "In this section we compare two approaches to {k} and measure the trade-offs.",
    "A common mistake is to optimize {k} before measuring real traffic.",
    "The simplest design for {k} is often the fastest one in practice.",
    "We benchmarked {k} on commodity hardware and the results were surprising.",
    "Good {t} engineering starts with clear goals for {k}.",
    "When {k} fails, the rest of the {t} stack usually fails with it.",
];

/// Builds one record from the RNG and its sequence number.
type RecordFn = fn(&mut ChaCha8Rng, usize) -> Value;

struct Args {
    out_dir: PathBuf,
    products: usize,
    movies: usize,
    articles: usize,
    catalog: Option<usize>,
    seed: u64,
}

fn parse_args() -> Args {
    let mut args = Args {
        out_dir: PathBuf::from("data"),
        products: 10_000,
        movies: 5_000,
        articles: 2_000,
        catalog: None,
        seed: 42,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut next_num = |name: &str| -> u64 {
            it.next()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("{name} expects a number"))
        };
        match arg.as_str() {
            "--products" => args.products = next_num("--products") as usize,
            "--movies" => args.movies = next_num("--movies") as usize,
            "--articles" => args.articles = next_num("--articles") as usize,
            "--seed" => args.seed = next_num("--seed"),
            "--catalog" => args.catalog = Some(next_num("--catalog") as usize),
            other if !other.starts_with("--") => args.out_dir = PathBuf::from(other),
            other => panic!("unknown flag {other}"),
        }
    }
    args
}

fn pick<'a, T: ?Sized>(rng: &mut ChaCha8Rng, items: &'a [&'a T]) -> &'a T {
    items.choose(rng).expect("non-empty list")
}

fn person(rng: &mut ChaCha8Rng) -> String {
    format!("{} {}", pick(rng, FIRST_NAMES), pick(rng, LAST_NAMES))
}

/// Rounds to 2 decimals so JSON output stays readable and stable.
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn product(rng: &mut ChaCha8Rng, id: usize) -> Value {
    let brand = pick(rng, BRANDS);
    let (category, nouns) = CATEGORIES.choose(rng).expect("non-empty");
    let noun = pick(rng, nouns);
    let adjective = pick(rng, ADJECTIVES);
    let color = pick(rng, COLORS);
    let mut features: Vec<&str> = FEATURES.choose_multiple(rng, 3).copied().collect();
    features.sort_unstable();
    let price = round2(rng.gen_range(5.0..1500.0_f64));
    json!({
        "objectID": format!("p{id}"),
        "name": format!("{brand} {adjective} {noun}"),
        "brand": brand,
        "category": category,
        "color": color,
        "price": price,
        "rating": round2(rng.gen_range(1.0..5.0_f64)),
        "reviews_count": rng.gen_range(0..20_000u32),
        "in_stock": rng.gen_bool(0.8),
        "description": format!(
            "The {brand} {adjective} {noun} in {color}: {}, {} and {}.",
            features[0], features[1], features[2]
        ),
        "features": features,
    })
}

fn movie(rng: &mut ChaCha8Rng, id: usize) -> Value {
    let title = if rng.gen_bool(0.3) {
        format!("The {} {}", pick(rng, TITLE_WORDS_A), pick(rng, TITLE_WORDS_B))
    } else {
        format!("{} {}", pick(rng, TITLE_WORDS_A), pick(rng, TITLE_WORDS_B))
    };
    let genre_count = rng.gen_range(1..=3);
    let mut genres: Vec<&str> = GENRES.choose_multiple(rng, genre_count).copied().collect();
    genres.sort_unstable();
    let actors: Vec<String> = (0..rng.gen_range(2..=4)).map(|_| person(rng)).collect();
    json!({
        "objectID": format!("m{id}"),
        "title": title,
        "year": rng.gen_range(1950..=2026u32),
        "genres": genres,
        "director": person(rng),
        "actors": actors,
        "rating": round2(rng.gen_range(1.0..10.0_f64)),
        "votes": rng.gen_range(10..2_000_000u32),
        "overview": format!(
            "{} {} {}",
            capitalize(pick(rng, PLOT_SUBJECTS)),
            pick(rng, PLOT_ACTIONS),
            pick(rng, PLOT_ENDINGS)
        ),
    })
}

fn article(rng: &mut ChaCha8Rng, id: usize) -> Value {
    let (topic, keywords) = TOPICS.choose(rng).expect("non-empty");
    let main_kw = pick(rng, keywords);
    let paragraphs: Vec<String> = (0..rng.gen_range(3..=6))
        .map(|_| {
            (0..rng.gen_range(3..=5))
                .map(|_| {
                    pick(rng, SENTENCE_TEMPLATES)
                        .replace("{k}", pick(rng, keywords))
                        .replace("{t}", topic)
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    let mut tags: Vec<&str> = keywords.choose_multiple(rng, 2).copied().collect();
    tags.push(topic);
    json!({
        "objectID": format!("a{id}"),
        "title": format!("A practical guide to {main_kw} in {topic}"),
        "author": person(rng),
        "topic": topic,
        "tags": tags,
        "published_at": format!(
            "{}-{:02}-{:02}",
            rng.gen_range(2015..=2026u32),
            rng.gen_range(1..=12u32),
            rng.gen_range(1..=28u32)
        ),
        "reading_minutes": paragraphs.len() as u32 * 2,
        "body": paragraphs.join("\n\n"),
    })
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn write_jsonl(
    path: &Path,
    count: usize,
    rng: &mut ChaCha8Rng,
    make: RecordFn,
) -> std::io::Result<()> {
    let mut out = BufWriter::new(File::create(path)?);
    for id in 0..count {
        serde_json::to_writer(&mut out, &make(rng, id))?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    println!("wrote {count:>6} records to {}", path.display());
    Ok(())
}

fn main() -> std::io::Result<()> {
    let args = parse_args();
    fs::create_dir_all(&args.out_dir)?;
    if let Some(records) = args.catalog {
        return catalog::write_catalog(&args.out_dir, records, args.seed);
    }
    // One RNG per dataset, so changing one count does not change the other files.
    let datasets: [(&str, usize, RecordFn); 3] = [
        ("products.jsonl", args.products, product),
        ("movies.jsonl", args.movies, movie),
        ("articles.jsonl", args.articles, article),
    ];
    for (i, (file, count, make)) in datasets.into_iter().enumerate() {
        let mut rng = ChaCha8Rng::seed_from_u64(args.seed.wrapping_add(i as u64));
        write_jsonl(&args.out_dir.join(file), count, &mut rng, make)?;
    }
    Ok(())
}
