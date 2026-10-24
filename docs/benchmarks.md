# Benchmarks

How fast the engine is on a realistic catalog, how that changes with size,
and what limits it. Measured on the development container: 4 CPU cores,
16 GB RAM, no swap. The load generator runs on the same machine as the
server, so HTTP numbers are a lower bound.

## Reproduce

```bash
cargo build --release --workspace
./target/release/datagen data/scale --catalog 100000     # records + 5,000 queries

# In-process: indexing time, memory, per-query latency, slowest queries
cargo run --release -p engine --example bench_queries -- \
  data/scale/catalog.jsonl config/catalog.json data/scale/catalog-queries.jsonl

# Stage-by-stage timing of single queries
cargo run --release -p engine --example profile_query -- \
  data/scale/catalog.jsonl config/catalog.json "stend s" "wireless"

# Over HTTP
SEARCH_ADMIN_KEY=a SEARCH_SEARCH_KEY=s ./target/release/server --in-memory \
  --port 9201 --load catalog=data/scale/catalog.jsonl,config/catalog.json &
./target/release/loadtest --url http://127.0.0.1:9201 --index catalog --key s \
  --queries data/scale/catalog-queries.jsonl --concurrency 1,4,16,64 --seconds 10
```

## The data

`datagen --catalog N` writes products with a large vocabulary: 40,000
invented words (description words follow a Zipf distribution, as in real
text), 2,000 brands, 3,000 product nouns, 300 three-level categories and
model numbers like `ZX-4821`. The 5,000 queries are sampled from records:
one to three words, 40% with a half-typed last word, 20% with a typo, 20%
with filters or facets.

The earlier sample data (`data/products.jsonl`) has only 141 distinct words,
which hid every cost that grows with the vocabulary.
