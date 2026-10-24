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

## Results (October 2026)

### Scale, in-process (one core, 5,000 sampled queries)

| | 100k records | 300k records | 1M records |
|---|---|---|---|
| Indexing time | 42 s | 134 s | 506 s |
| Indexing rate | 2,400 rec/s | 2,200 rec/s | 2,000 rec/s |
| Memory (resident) | 958 MB | 2,581 MB | 8,086 MB |
| Distinct words / prefixes | 55,443 / 223,645 | 55,567 / 224,205 | 55,579 / 224,261 |
| Query mean | 6.0 ms | 9.9 ms | 24.3 ms |
| Query p50 | 5.5 ms | 7.4 ms | 10.8 ms |
| Query p90 | 10.5 ms | 20.9 ms | 65.6 ms |
| Query p99 | 17.6 ms | 42.5 ms | 127.4 ms |
| Query max | 32.8 ms | 69.4 ms | 226.2 ms |

Indexing time and memory grow linearly, about 8 KB per record. The median
query grows slowly; the tail grows with the number of hits.

### HTTP, 100k records (server and load generator share 4 cores)

| Concurrency | Requests/s | p50 | p90 | p99 | max | Errors |
|---|---|---|---|---|---|---|
| 1 | 127 | 7.1 ms | 13.6 ms | 21.0 ms | 46.2 ms | 0 |
| 4 | 531 | 6.9 ms | 12.7 ms | 20.2 ms | 45.6 ms | 0 |
| 16 | 587 | 27.2 ms | 42.0 ms | 53.8 ms | 71.8 ms | 0 |
| 64 | 582 | 106.7 ms | 169.1 ms | 233.2 ms | 363.1 ms | 0 |

Throughput scales from 1 to 4 workers and then stays at about 580 requests/s:
the cores are full and further requests wait in line. The server's own
`processingTimeMS` stays at 5–6 ms, so search work, not HTTP, is the limit.

### Where the time goes (300k records, `profile_query`)

| Query | Hits | Matched words | Planning | Ranking | Total |
|---|---|---|---|---|---|
| `kopl` (common word) | 291,163 | 68 | 0.7 ms | 33.3 ms | 37.4 ms |
| `s` | 288,657 | 5,799 | 0.9 ms | 38.2 ms | 28.0 ms |
| `zost s` | 3,213 | 5,810 | 1.9 ms | 38.8 ms | 34.7 ms |
| `goul` (typo allowed) | 15,957 | 173 | 2.8 ms | 4.1 ms | 7.2 ms |
| `plioskozobrel` (2 typos allowed) | 17,433 | 1 | 5.2 ms | 1.0 ms | 5.8 ms |

(Stages are timed separately, so they do not add up exactly.)

## Findings

1. **Huge result sets.** Queries matching a large share of the catalog
   (`s`, `p`, a very common word: 600k–990k hits at 1M) take 160–226 ms.
   The staged ranking still computes each criterion record by record. Fix:
   split groups with bitmap operations (records with 0 typos, records
   matching in attribute 0, ...) and keep a precomputed custom-ranking order,
   so the cost no longer grows with the number of hits.
2. **Short prefixes inside multi-word queries.** In `zost s`, the `s`
   expands to 5,810 words; proximity reads positions for all of them
   (195 ms at 1M). Fix: look positions up per record (a per-record word
   list) when a term has many words, instead of per word.
3. **Typo expansion grows with the vocabulary**: 3–5 ms of planning for
   words long enough for two typos, with 55k distinct words.
4. **Indexing at about 2,000 records/s**, dominated by updating every prefix
   of every word for each record.
5. **Memory at about 8 KB per record**, mostly word positions in per-word
   hash maps and records kept as JSON trees.
