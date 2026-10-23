# search-engine

An Algolia-style search engine written from scratch in Rust.

## Layout

- `engine/` – the search engine library
- `datagen/` – generates deterministic sample data (products, movies, articles)

## Quick start

```bash
cargo run --release -p datagen -- data   # writes data/*.jsonl
cargo test
cargo run --release -p engine --bin search -- data/products.jsonl "wireless head"
```

See [ROADMAP.md](ROADMAP.md) for the plan and open items.

Datagen flags: `--products N --movies N --articles N --seed S`.
