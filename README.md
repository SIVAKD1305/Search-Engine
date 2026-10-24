# Search-Engine

AI Search-Engine.

An Algolia-style search engine written from scratch in Rust.

## Layout

- `engine/` – the search engine library
- `server/` – HTTP server with an Algolia-style REST API
- `datagen/` – generates deterministic sample data (products, movies, articles)
- `config/` – index settings for the sample datasets

## Quick start

```bash
cargo run --release -p datagen -- data   # writes data/*.jsonl
cargo test
cargo run --release -p engine --bin search -- data/products.jsonl "wireless head"
```

## Run the server

```bash
SEARCH_ADMIN_KEY=my-admin SEARCH_SEARCH_KEY=my-search \
  cargo run --release -p server -- --port 8080 \
  --load products=data/products.jsonl,config/products.json

curl -H 'X-Algolia-API-Key: my-search' -X POST \
  http://127.0.0.1:8080/1/indexes/products/query \
  -d '{"query": "wirless head", "filters": "price < 100", "facets": ["brand"]}'
```

Without the environment variables, random keys are generated and printed.

Data is saved in `./search-data` (change with `--data-dir DIR`, or use
`--in-memory`) and loaded again at startup. Every write is in the index's
write-ahead log on disk before the server answers, so it survives a crash.
`--load` replaces an index with the records of a JSONL file.

## Chinese and Japanese

Text without spaces is split into words with a dictionary: jieba for
Chinese, Lindera with IPADIC for Japanese. Text containing kana is treated
as Japanese; text in Han characters only is treated as Chinese unless the
index is marked as Japanese, which a Japanese index should be, so that
queries typed in kanji only are split the same way as the records:

```bash
curl -H 'X-Algolia-API-Key: my-admin' -X PUT \
  http://127.0.0.1:8080/1/indexes/shop/settings -d '{"indexLanguages": ["ja"]}'
```

Long words are also indexed by their parts (関西国際空港 is found by 空港),
half-typed words are matched as prefixes, and Chinese and Japanese words get
no typo tolerance. Full-width letters match ASCII (Ｗｉｆｉ = wifi).

The first build downloads the Japanese dictionary from lindera.dev (about
50 MB, embedded in the binary). To build without network access, point
`LINDERA_BUILD_DICTIONARY_CACHE_DIR` at a directory holding a previously
built dictionary, or leave Japanese out with
`cargo build --release -p server --no-default-features --features chinese`;
without a dictionary each character is a word.

## Read replicas

Start more servers that copy everything from a primary and serve searches:

```bash
SEARCH_ADMIN_KEY=replica-admin SEARCH_SEARCH_KEY=my-search \
SEARCH_PRIMARY_ADMIN_KEY=my-admin \
  cargo run --release -p server -- --port 8081 --data-dir replica-data \
  --replica-of http://127.0.0.1:8080
```

A replica copies each index, then follows every change (usually within a few
milliseconds). It keeps serving searches when the primary is down and catches
up when it returns. Writes sent to a replica are refused with `403`. Check
lag with `GET /1/replication/status` (admin key).

| Method | Path | Key |
|---|---|---|
| POST | `/1/indexes/{index}/query` | search |
| GET | `/1/indexes/{index}?query=...` | search |
| POST | `/1/indexes/*/queries` | search |
| GET | `/1/indexes` | admin |
| POST / DELETE | `/1/indexes/{index}` (add record / delete index) | admin |
| POST | `/1/indexes/{index}/batch` | admin |
| POST | `/1/indexes/{index}/clear` | admin |
| GET / PUT | `/1/indexes/{index}/settings` | admin |
| GET / PUT / DELETE | `/1/indexes/{index}/{objectID}` | admin |
| POST | `/1/indexes/{index}/{objectID}/partial` | admin |
| GET | `/1/indexes/{index}/task/{taskID}` | admin |
| GET | `/1/isalive` | none |

See [ROADMAP.md](ROADMAP.md) for the plan and open items.

Datagen flags: `--products N --movies N --articles N --seed S`.
