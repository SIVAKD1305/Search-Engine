# Search-Engine

An Algolia-style search engine written from scratch in Rust: engine library,
HTTP server with Algolia's REST API, read replicas, and a Raft cluster with
automatic failover. Built step by step; `ROADMAP.md` lists what is done
(steps 1-11c) and the known gaps, `docs/benchmarks.md` every measurement.

## Layout

- `engine/` - the library: `index.rs` (inverted index, prefix sets, images),
  `search.rs` (query planning, filters, facets, geo), `bucket.rs` (staged
  ranking), `ranking.rs`, `typo.rs`, `tokenizer.rs` (incl. Chinese/Japanese
  segmentation), `facets.rs`, `geo.rs`, `synonyms.rs`, `highlight.rs`.
  `engine/examples/` holds benchmark and profiling tools.
- `server/` - axum HTTP server: `lib.rs` (handlers, auth), `state.rs`
  (indexes, group commit, cluster commands), `storage.rs` (snapshot + WAL +
  image), `keys.rs` (API keys, secured keys, rate limits), `replication.rs`
  (read replicas), `raft.rs` (pure Raft core), `raft_store.rs`,
  `cluster.rs` (Raft driver). Integration tests in `server/tests/`.
- `datagen/` - test data (`--catalog N` for large catalogs); `loadtest/` -
  HTTP load generator (`--write` for writes); `config/` - index settings.

## Working here

- Build and test: `cargo build --release --workspace`,
  `cargo test --release --workspace`, and
  `cargo clippy --release --workspace --all-targets` must be clean.
- Formatting follows the existing code (wide lines); do not run `cargo fmt`.
- Test data is not committed: `./target/release/datagen data` and
  `./target/release/datagen data/scale --catalog 100000`.
- The Japanese dictionary (Lindera IPADIC) is downloaded from lindera.dev on
  the first build. Without network access, build with
  `--no-default-features --features chinese` (server) or set
  `LINDERA_BUILD_DICTIONARY_CACHE_DIR` to a prebuilt dictionary.
- Raft safety tests run 40 random seeds by default; `RAFT_SEEDS=2000` for a
  longer run after changing `raft.rs`.
- Measure before and after performance changes (`bench_queries`,
  `loadtest`) and record results in `docs/benchmarks.md`; keep
  `ROADMAP.md` up to date when a step is done.
- Commit each finished step to `main` with a message saying what changed
  and why, with the numbers.

## Next candidates

From ROADMAP.md: adding and removing cluster nodes while running (Raft
membership changes), a load test across separate machines, approximate
facet counts on very large results, memory-mapped images for near-instant
startup at 1M records.
