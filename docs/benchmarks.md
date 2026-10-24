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

# Restart time: start with a data directory, stop with Ctrl-C or SIGTERM
# (which checkpoints), start again and read "index(es) loaded in ... ms"
SEARCH_ADMIN_KEY=a SEARCH_SEARCH_KEY=s ./target/release/server --data-dir /tmp/sd \
  --port 9201 --load catalog=data/scale/catalog.jsonl,config/catalog.json
```

Write throughput: `loadtest --write --url ... --index catalog --key ADMIN_KEY`
(the server must use a data directory, so writes are fsynced).

## The data

`datagen --catalog N` writes products with a large vocabulary: 40,000
invented words (description words follow a Zipf distribution, as in real
text), 2,000 brands, 3,000 product nouns, 300 three-level categories and
model numbers like `ZX-4821`. The 5,000 queries are sampled from records:
one to three words, 40% with a half-typed last word, 20% with a typo, 20%
with filters or facets.

The earlier sample data (`data/products.jsonl`) has only 141 distinct words,
which hid every cost that grows with the vocabulary.

## After the ground work (items 1-5 of the roadmap)

Same machine, same data and queries as the first measurements below.

| | 100k before | 100k now | 1M before | 1M now |
|---|---|---|---|---|
| Indexing time | 42 s | 13.8 s | 506 s | 183 s |
| Memory (resident) | 958 MB | 283 MB | 8,086 MB | 2,281 MB |
| Query mean | 6.0 ms | 1.8 ms | 24.3 ms | 4.8 ms |
| Query p50 | 5.5 ms | 1.7 ms | 10.8 ms | 2.8 ms |
| Query p99 | 17.6 ms | 5.2 ms | 127.4 ms | 32.9 ms |
| Query max | 32.8 ms | (not measured) | 226.2 ms | 71.0 ms |
| Queries/s, one core | 167 | 555 | 41 | 209 |
| Restart | 13 s | 0.7 s | 190 s | 11-14 s |

Restart "before" re-indexes every record from the snapshot; "now" loads the
index image written next to it (100k: 170 MB, 1M: 1.6 GB). Answers to 300
sampled queries were byte-for-byte the same either way.

Writes over HTTP at 100k records (`loadtest --write`, each request one
record, acknowledged after fsync):

| Writers | Before group commit | With group commit |
|---|---|---|
| 1 | 1,695 writes/s | 1,360 writes/s |
| 16 | 1,891 writes/s | 5,068 writes/s |
| 64 | 1,791 writes/s | 4,782 writes/s |

The slowest queries at 1M are all facet counts over 150k-220k hits
(`brand` and `category` together, 45-70 ms). Approximate counts on very
large results, as Algolia does, are the next fix there.

## Raft cluster (11b)

Three server processes on one machine (4 cores shared with the load
generator), the 100k catalog plus earlier test records (117k). Every write
is answered after a majority fsynced it and the answering node applied it.

| Writers | Through the leader | Through a follower |
|---|---|---|
| 1 | 453 writes/s (p50 1.8 ms) | 413 writes/s (p50 2.2 ms) |
| 16 | 3,809 writes/s | 2,780 writes/s |
| 64 | 7,573 writes/s | 4,613 writes/s |

Loading the 100k catalog through the cluster takes 30 s (each node indexes
every record). Failover: with one client writing through a follower, the
leader was killed with `kill -9`; the longest gap between successful writes
was 1.76 s, one write in flight answered 503 (it was applied after all),
and all 1,778 acknowledged writes were on every node, including the old
leader after it restarted.

Two fixes came out of measuring this. Followers learned that an entry was
committed only from the next heartbeat (100 ms per forwarded write); the
leader now tells them as soon as it commits. And replacing a record that
has numeric attributes scanned a whole bucket of the range-filter index
(2.56 ms per update at 100k records); it now takes 0.18 ms, as adding one
does, standalone too.

### Snapshot restores (11c)

Same cluster. Cluster snapshots now carry index images.

| | Before | After |
|---|---|---|
| Node restart (load the snapshot) | 12.8-17.5 s | 0.53-0.78 s |
| Follower 31,000 entries behind: restart and catch up | about 15 s (re-index) | 4.0 s |
| Writes held up by each snapshot (every 10,000 entries) | 0.34-1.39 s | 0.12-0.16 s |
| Slowest write at 16 writers | 3.25 s | 0.27-0.46 s |

The image (172 MB) is sent with the snapshot (57 MB) to followers that are
far behind; saving both takes 1.5-4.6 s on a writer thread, while writes go
on. The first version serialized records and images on the applier,
which made the pause longer (up to 1.4 s) before the copy was moved off it.

## Geo search (item 8)

The 100k catalog with a `_geoloc` for every record, around 50 random
cities (spread about 30 km), and the same 5,000 queries, each with an
`aroundLatLng` near a city: half with `aroundRadius: 50000`, half with the
automatic radius.

| | Mean | p50 | p99 |
|---|---|---|---|
| Without geo | 1.77 ms | 1.61 ms | 5.53 ms |
| Radius 50 km | 1.85 ms | 1.80 ms | 4.43 ms |
| Automatic radius | 2.11 ms | 1.71 ms | 10.08 ms |
| Automatic, measuring every match (first version) | 15.65 ms | 15.21 ms | 28.57 ms |

The first version measured every matching record's distance up to three
times. Now the nearest records are found by searching rings of grid cells
outward from the point, until nothing outside can be nearer.

## Chinese and Japanese (item 6)

50,000 generated product records per language: names and descriptions made
of the 20,000 most common words of each dictionary (IPADIC nouns, jieba's
word list), with Japanese particles and endings. Queries are the first 2-8
characters of a random product name, so many end inside a word, as when
typing.

| | Japanese | Chinese |
|---|---|---|
| Indexing | 4.9 s (10,200 rec/s) | 2.5 s (20,000 rec/s) |
| Memory | 97 MB | 111 MB |
| Query mean / p99 | 0.82 / 15.4 ms | 0.99 / 18.2 ms |
| Typed queries without results | 0 / 300 | 1 / 300 |

The dictionaries cost memory only when used: about 3 MB for Japanese (read
from the binary) and 33 MB for Chinese (jieba builds its word trie at first
use, in about 0.1 s). The server binary grows from 42 to 111 MB, mostly the
embedded Japanese dictionary. English results are unchanged (100k catalog:
indexing 12.1 s, mean 1.70 ms, p99 5.06 ms).

## First measurements (October 2026)

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

## Findings (from the first measurements; fixed in items 3-5)

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
