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

## Next

9. Persistence to disk
10. Plurals, synonyms, stop words, optional words
11. Replication and multi-node

## Remember for later

- Highlighting cost: highlighting long attributes (article bodies) adds about
  1 ms for 5 hits. Snippet window search allocates a set per window; use a
  sliding count instead. Consider highlighting only the snippet window when
  an attribute is snippeted.
- Server gaps: data is in memory only (step 9); writes hold the index's write
  lock while applying, so a big batch pauses searches on that index (Algolia
  runs indexing in a separate process); no secured (HMAC-restricted) API
  keys, rate limiting, browse/cursor endpoint, or replica indexes yet.
- Load test: 2,000 searches with 16 parallel `curl` processes on 4 cores
  gave p50 4.6 ms, p99 27.6 ms, 315 req/s, all 200s. The client (one process
  per request) was the bottleneck; measure again with a real load generator.
- Facets not yet done: `facets_stats` (min/max/avg of numeric facets),
  searching inside facet values (`searchForFacetValues`), nested attributes
  (`brand.name`), and geo filters (`aroundLatLng`, the `geo` criterion).

- **Proper Chinese/Japanese word splitting.** The tokenizer currently makes
  each Han/Kana character its own token (`engine/src/tokenizer.rs`,
  `is_unspaced_script`). Replace it with a dictionary-based segmenter such as
  `lindera` (Japanese, also Chinese/Korean dictionaries) or `jieba-rs`
  (Chinese). Korean is space-separated and already works at word level.
- Ranking cost on big result sets: every candidate is fully scored and
  sorted, so "h" (4,772 hits) takes about 7.6 ms and the empty query (10,000)
  about 11 ms. Do what Algolia does: apply criteria lazily as a bucket sort
  (split by typo count with bitmaps first, score deeper only inside the
  buckets needed for the requested page), plus a fast path for the empty query.
- Typo-tolerance timings in step 4 were measured with prices indexed (8,964
  words). With `config/products.json` the dictionary has 141 words, so
  re-measure on a large real vocabulary before trusting the speed.
- Indexing speed: 10k products take about 0.7 s. Profile before optimizing
  (JSON parsing, record clone in `Index::add`, `HashMap` per record).
- Typo lookup speed: words with 2 allowed typos (8+ letters) take about
  0.3 ms on a 9k-word dictionary and grow with dictionary size. Options: a
  Levenshtein automaton over an FST (`fst` crate), or caching expansions of
  frequent query words.
