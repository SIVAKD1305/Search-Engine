# Roadmap

## Done

1. Workspace, document store, sample data generator
2. Tokenizer: lowercase, accent folding, positions, byte offsets
3. Inverted index with per-prefix record sets; AND matching with prefix on the last word
4. Typo tolerance: Damerau-Levenshtein trie walk over the sorted dictionary,
   prefix-aware, first-letter typo costs double, Algolia's 4/8 thresholds
5. Tie-breaking ranking (typo, words, proximity, attribute, exact, custom),
   JSON settings with Algolia names, pagination, per-dataset configs in `config/`
6. Filters (Algolia `filters` syntax: facets, numbers, ranges, AND/OR/NOT),
   facet counts, `filterOnly(...)`, `optionalFilters` and the `filters` criterion
7. Highlighting (`_highlightResult`: typo- and prefix-aware, accent-safe,
   HTML-escaped) and snippets (`_snippetResult`, best centered window)
8. HTTP server (`server/`): Algolia REST paths and JSON, `params` strings,
   multi-query, batch, partial updates, settings merge, admin/search keys, CORS
9. Persistence: per-index snapshot + checksummed write-ahead log, fsync before
   acknowledging, torn-tail recovery, compaction into a new generation
10. Query understanding: `ignorePlurals` (English), `synonyms` (regular and
    one-way), `removeStopWords`, `removeWordsIfNoResults` (lastWords,
    firstWords, allOptional) with `queryAfterRemoval`, `optionalWords`
11a. Read replicas (like Algolia's DSN): numbered change log with index
    epochs, `--replica-of`, log shipping with long-poll, snapshot copy when
    too far behind, replicas persist, refuse writes, report lag

## Ground work (before 11b)

1. ✅ Speed on large result sets: staged ranking (bucket sort) that refines only
   the groups that can reach the requested page, custom-ranking values in
   columns, per-attribute word and prefix sets. Checked against the full
   ranking by a randomized test. 10k products: empty query 6.7 → 0.34 ms,
   "s" (8,242 hits) 7.7 → 1.0 ms, "h" (4,772 hits) 4.7 → 0.7 ms; memory
   40 → 46 MB.
2. ✅ Load test and large dataset: `datagen --catalog N`, `bench_queries`,
   `profile_query`, `loadtest`. Results and findings in
   [docs/benchmarks.md](docs/benchmarks.md): 1M records index in 506 s, use
   8.1 GB, p50 10.8 ms, p99 127 ms; about 580 HTTP requests/s at 100k.
3. ✅ Query time at scale: per-record word lists instead of per-word position
   maps, ranking with set operations, a custom-ranking order kept sorted on
   every write, facet counts per record for small results (100k catalog:
   p99 17.6 -> 12.9 ms, memory 958 -> 721 MB)
4. ✅ Typo and filter speed: first-letter typo budget prunes the dictionary
   walk; prefix matches reported as groups (one precomputed set instead of
   hundreds of words); highlighting skips attributes without a match; ASCII
   fast path in the tokenizer; numeric range filters from three levels of
   precomputed buckets (100k catalog: mean 5.1 -> 1.9 ms, p99 12.9 -> 5.4 ms,
   529 queries/s per core)
5. Indexing speed and memory; group commit for single writes; faster
   startup (findings 4 and 5)
6. Chinese/Japanese word segmentation
7. Secured API keys and rate limiting
8. Geo search, facet stats, multi-word synonyms

## Next

11b. Automatic failover: 3 nodes agree on every write (Raft) and elect a new
    primary when one fails

## Remember for later

- Highlighting cost: highlighting long attributes (article bodies) adds about
  1 ms for 5 hits. Snippet window search allocates a set per window; use a
  sliding count instead. Consider highlighting only the snippet window when
  an attribute is snippeted.
- Startup re-indexes every record from the snapshot (10k products: about
  0.6 s). For large indexes, save the built index structures (dictionary,
  postings, bitmaps) instead of only the records, e.g. as memory-mapped files.
- Each single-record write waits for its own fsync (about 11 ms per request in
  this container, including curl). Group commit (one fsync for writes that
  arrive together) would raise write throughput; batches already share one.
- Replication gaps: a replica that was disconnected retries with backoff
  up to 10 s, so catch-up after an outage can take that long; replicas use the
  primary's admin key (a dedicated replication key would be safer); the
  primary is a single point of failure for writes until 11b; replication
  traffic is plain HTTP unless the primary URL is https.
- Server gaps: applying a write holds the index's write lock, so a big batch
  pauses searches on that index while it is applied in memory (the fsync
  itself does not block searches; Algolia runs indexing in a separate
  process); no secured (HMAC-restricted) API
  keys, rate limiting, browse/cursor endpoint, or replica indexes yet.
- Load test: 2,000 searches with 16 parallel `curl` processes on 4 cores
  gave p50 4.6 ms, p99 27.6 ms, 315 req/s, all 200s. The client (one process
  per request) was the bottleneck; measure again with a real load generator.
- Query understanding not yet done: multi-word synonyms ("new york" = "ny"),
  `altCorrection` synonyms and placeholders, Algolia's separate
  `/synonyms` API (here synonyms are a setting), plurals and stop words for
  languages other than English (`queryLanguages`), and splitting or joining
  words ("head phones" = "headphones").
- Facet counts on very large results (30k+ hits over 2,000 brands) take
  5–9 ms; Algolia approximates counts there (`exhaustiveFacetsCount: false`).
- Facets not yet done: `facets_stats` (min/max/avg of numeric facets),
  searching inside facet values (`searchForFacetValues`), nested attributes
  (`brand.name`), and geo filters (`aroundLatLng`, the `geo` criterion).

- **Proper Chinese/Japanese word splitting.** The tokenizer currently makes
  each Han/Kana character its own token (`engine/src/tokenizer.rs`,
  `is_unspaced_script`). Replace it with a dictionary-based segmenter such as
  `lindera` (Japanese, also Chinese/Korean dictionaries) or `jieba-rs`
  (Chinese). Korean is space-separated and already works at word level.
- Typo-tolerance timings in step 4 were measured with prices indexed (8,964
  words). With `config/products.json` the dictionary has 141 words, so
  re-measure on a large real vocabulary before trusting the speed.
- Indexing speed: 10k products take about 0.7 s. Profile before optimizing
  (JSON parsing, record clone in `Index::add`, `HashMap` per record).
- Typo lookup speed: words with 2 allowed typos (8+ letters) take about
  0.3 ms on a 9k-word dictionary and grow with dictionary size. Options: a
  Levenshtein automaton over an FST (`fst` crate), or caching expansions of
  frequent query words.
