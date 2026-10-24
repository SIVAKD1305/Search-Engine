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
5. ✅ Indexing speed, memory and startup: records stored as compact JSON
   text, capped prefix sets (up to 4 characters), occurrence lists instead of
   per-word position maps; group commit (one fsync for all writes that
   arrive together); an index image next to each snapshot so startup loads
   the built index instead of re-indexing, and a checkpoint on shutdown.
   100k catalog: indexing 38.6 -> 13.8 s, memory 958 -> 283 MB, restart
   13.0 -> 0.7 s, 16 concurrent writers 1,891 -> 5,068 writes/s. 1M catalog:
   indexing 506 -> 183 s, memory 8.1 -> 2.3 GB, p99 127 -> 33 ms, restart
   190 -> 11-14 s.
6. ✅ Chinese/Japanese word segmentation: jieba (Chinese) and Lindera with
   IPADIC (Japanese), chosen by script (kana means Japanese) and by
   `indexLanguages`; dictionary words inside long words indexed as parts;
   query fallbacks for half-typed words (joined back, or split into a word
   and a prefix) and for words found only as parts; no typos for CJK words;
   voicing marks kept and full-width forms folded. Typed queries (first 2-8
   characters of a product name, 50k records each): 0/300 Japanese and 1/300
   Chinese without results (12 and 11 before the fallbacks).
7. ✅ API keys: `/1/keys` with Algolia's ACLs, index patterns, validity,
   `maxQueriesPerIPPerHour`, `maxHitsPerQuery`, forced `queryParameters`
   and referers, saved in `keys.json` and copied to replicas; secured API
   keys with Algolia's HMAC format (`filters`, `validUntil`,
   `restrictIndices`, `restrictSources`, `userToken`, forced parameters),
   whose filters are applied as separate conditions so a request cannot
   lift them; sliding-window rate limits per key and IP (or userToken), and
   per IP for unknown keys; `server secured-key` to make secured keys.
8. ✅ Geo search, facet stats, multi-word synonyms:
   - `_geoloc` (one point or several), `aroundLatLng` with `aroundRadius`
     (meters, `"all"`, or automatic: the 1,000 nearest matching records),
     `aroundPrecision`, `minimumAroundRadius`, `insideBoundingBox`,
     `insidePolygon`; the `geo` criterion (after `typo`, as in Algolia),
     `geoDistance` and `matchedGeoLocation` in `_rankingInfo`. Points are in
     a grid of 0.25° cells, and the nearest records are found by searching
     rings of cells outward (100k records in 50 clusters: mean 2.1 ms,
     p99 8.3 ms with a point in every query).
   - `facets_stats` (min, max, avg, sum) for numeric facets.
   - Synonym entries of up to 5 words: a word matches a phrase of adjacent
     words, and query words forming an entry match its alternatives;
     Chinese/Japanese entries work however they are split.

## Done after the ground work

11b. ✅ Automatic failover with Raft (`server/src/raft.rs`, `cluster.rs`):
    3 or 5 nodes agree on every write; a write is answered once a majority
    has it on disk and the answering node has applied it; followers forward
    writes to the leader; every node serves searches. Pre-Vote, leader
    leases and check quorum keep a returning or cut-off node from disrupting
    the cluster. The core is a pure state machine tested in a simulated
    faulty network (crashes, partitions, loss, duplication, reordering;
    2,000 seeds). Three processes with 100k records: leader killed with
    `kill -9` under load, writes back within 1.8 s, no acknowledged write
    lost; 453 writes/s from one writer, 7,573/s with 64.

## Next

- Not decided yet.

## Remember for later

- Highlighting cost: highlighting long attributes (article bodies) adds about
  1 ms for 5 hits. Snippet window search allocates a set per window; use a
  sliding count instead. Consider highlighting only the snippet window when
  an attribute is snippeted.
- Startup at 1M records takes 11-14 s even from the image (1.6 GB, decoded
  into many small allocations). Memory-mapping a flat layout would make it
  near instant. Compaction (every 10,000 writes) writes the snapshot and
  image while holding the write log: writes pause about 1.7 s at 100k
  records (searches continue). Write snapshots in the background from a
  copy-on-write view instead.
- Group commit raised write throughput 2.7x at 16 writers; a single writer
  still waits for its own fsync (about 0.7 ms here).
- Replication gaps: a replica that was disconnected retries with backoff
  up to 10 s, so catch-up after an outage can take that long; replicas use the
  primary's admin key (a dedicated replication key would be safer);
  replication traffic is plain HTTP unless the primary URL is https.
- Cluster gaps: membership is fixed at startup (no adding or removing
  nodes while running); a snapshot pauses applying on that node while the
  state is exported (writes wait up to about 0.7 s at 117k records, every
  10,000 entries); restoring a snapshot re-indexes every record (13-17 s
  at 100k: add index images to cluster snapshots); snapshots are sent and
  received whole in memory; searches read the local copy, so they can lag
  the leader slightly (no linearizable reads); a write whose node loses
  the leader mid-way answers 503 although it may still be applied (writes
  are not retried automatically, as `clear` is not idempotent); nodes talk
  plain HTTP authenticated with the shared admin key; rate limit counters
  are per node; read replicas following a cluster node are untested.
- Server gaps: applying a write holds the index's write lock, so a big batch
  pauses searches on that index while it is applied in memory (the fsync
  itself does not block searches; Algolia runs indexing in a separate
  process); no browse/cursor endpoint or replica indexes yet.
- Key gaps: rate limit counters are per server (each replica counts on its
  own); a key's or secured key's filters are evaluated on every query
  (a `price < 500` filter over 100k records cost about 20% of throughput;
  cache the record set per index version); a secured key's parent is found
  by trying each search key's HMAC once, then cached; `validity` is
  reported as set, not as time left; no `/1/keys/{key}/restore`; reading
  records by objectID is refused for keys that force filters or
  `attributesToRetrieve`.
- Load test: 2,000 searches with 16 parallel `curl` processes on 4 cores
  gave p50 4.6 ms, p99 27.6 ms, 315 req/s, all 200s. The client (one process
  per request) was the bottleneck; measure again with a real load generator.
- Query understanding not yet done: `altCorrection` synonyms and
  placeholders, Algolia's separate
  `/synonyms` API (here synonyms are a setting), plurals and stop words for
  languages other than English (`queryLanguages`), and splitting or joining
  words ("head phones" = "headphones").
- Facet counts on very large results (30k+ hits over 2,000 brands) take
  5–9 ms; Algolia approximates counts there (`exhaustiveFacetsCount: false`).
- Facets not yet done: searching inside facet values
  (`searchForFacetValues`) and nested attributes (`brand.name`).
- Geo gaps: boxes and polygons must not cross the 180th meridian;
  `aroundPrecision` as a list of ranges, `aroundLatLngViaIP` and
  `aroundRadius` per query language are not supported; the automatic
  radius counts the nearest 1,000 matching records rather than Algolia's
  density estimate; when a query matches most of a large index, choosing
  the automatic radius measures every match (about 6 ms at 100k).
- Synonym gaps: a synonym applies only to whole query words (not to the
  word being typed, unless it is complete); a query phrase matched through
  its words anywhere also counts its words as one query term for ranking.

- Chinese/Japanese gaps: a query starting in the middle of a word finds
  nothing (as in English: "eadphones"); multi-word synonyms are needed for
  CJK synonyms that split into several words ("笔记本电脑"); a Japanese index
  must set `indexLanguages: ["ja"]` or kanji-only queries are split as
  Chinese; Korean is split at spaces only, so particles stay attached
  (서울에서 does not match 서울); traditional and simplified Chinese are not
  mapped to each other; the Japanese dictionary is downloaded at build time
  and adds about 50 MB to the binary.
- Typo-tolerance timings in step 4 were measured with prices indexed (8,964
  words). With `config/products.json` the dictionary has 141 words, so
  re-measure on a large real vocabulary before trusting the speed.
- Indexing speed: 10k products take about 0.7 s. Profile before optimizing
  (JSON parsing, record clone in `Index::add`, `HashMap` per record).
- Typo lookup speed: words with 2 allowed typos (8+ letters) take about
  0.3 ms on a 9k-word dictionary and grow with dictionary size. Options: a
  Levenshtein automaton over an FST (`fst` crate), or caching expansions of
  frequent query words.
