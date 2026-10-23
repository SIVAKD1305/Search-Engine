# search-engine

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
