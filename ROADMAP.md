# Roadmap

## Done

1. Workspace, document store, sample data generator
2. Tokenizer: lowercase, accent folding, positions, byte offsets
3. Inverted index with per-prefix record sets; AND matching with prefix on the last word

## Next

4. Typo tolerance (Damerau-Levenshtein over the dictionary)
5. Tie-breaking ranking: typo, words, proximity, attribute, exact, custom
6. Filters and facets
7. Highlighting and snippets
8. HTTP API (Algolia-like JSON)
9. Persistence to disk
10. Plurals, synonyms, stop words, optional words
11. Replication and multi-node

## Remember for later

- **Proper Chinese/Japanese word splitting.** The tokenizer currently makes
  each Han/Kana character its own token (`engine/src/tokenizer.rs`,
  `is_unspaced_script`). Replace it with a dictionary-based segmenter such as
  `lindera` (Japanese, also Chinese/Korean dictionaries) or `jieba-rs`
  (Chinese). Korean is space-separated and already works at word level.
- Index only the attributes that should be searchable: with default settings,
  numbers like `price` are indexed too (products: 8,964 words, mostly numbers).
- Indexing speed: 10k products take about 0.7 s. Profile before optimizing
  (JSON parsing, record clone in `Index::add`, `HashMap` per record).
