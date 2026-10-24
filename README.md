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

## Geo search, facet stats and synonyms

Records with `_geoloc` (`{"lat": 48.85, "lng": 2.35}`, or a list) can be
searched around a point, inside boxes or polygons, and ranked by distance:

```bash
curl -H 'X-Algolia-API-Key: my-search' -X POST \
  http://127.0.0.1:8080/1/indexes/shops/query \
  -d '{"query": "pizza", "aroundLatLng": "48.85, 2.35", "aroundRadius": 5000}'
```

Without `aroundRadius` the radius is chosen to cover the 1,000 nearest
matches. Numeric facets get `facets_stats` (min, max, avg, sum). Synonyms
may be phrases: `{"type": "synonym", "synonyms": ["ny", "new york"]}`.

## API keys

The admin key can do everything; the search key can search. More keys,
with Algolia's rights and limits, are made with the admin key:

```bash
curl -H 'X-Algolia-API-Key: my-admin' -X POST http://127.0.0.1:8080/1/keys \
  -d '{"acl": ["search"], "indexes": ["products"], "maxQueriesPerIPPerHour": 1000}'
```

A backend can hand each user a **secured key** made from a search key,
without calling the server, the same way as Algolia's
`generateSecuredApiKey` (so Algolia client libraries can make them too):

```bash
cargo run --release -p server -- secured-key SEARCH_KEY \
  'filters=owner%3A42&validUntil=1767225600&userToken=42'
```

Its filters always apply on top of the request's, it can only search, and
it expires at `validUntil`. Past `maxQueriesPerIPPerHour` (counted per
`userToken` for secured keys) requests get `429` with `Retry-After`. Behind a
proxy, start the server with `--trust-proxy` so client addresses come from
`X-Forwarded-For`.

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

## Cluster with automatic failover

Three (or five) nodes agree on every write with Raft. Writes can go to any
node (followers forward them to the leader) and are answered once a
majority has them on disk; every node serves searches. If the leader
fails, the others elect a new one in about a second and carry on; a node
that comes back catches up on its own.

```bash
cargo build --release -p server
export SEARCH_ADMIN_KEY=my-admin SEARCH_SEARCH_KEY=my-search
M=a=http://127.0.0.1:7001,b=http://127.0.0.1:7002,c=http://127.0.0.1:7003
./target/release/server --port 7001 --data-dir data-a --cluster $M --node-id a &
./target/release/server --port 7002 --data-dir data-b --cluster $M --node-id b &
./target/release/server --port 7003 --data-dir data-c --cluster $M --node-id c &
curl -H 'X-Algolia-API-Key: my-admin' http://127.0.0.1:7001/1/cluster/status
```

All nodes share the admin key. With three nodes, one can be down; with
five, two.

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

Keys made with `/1/keys` can use the endpoints their ACL allows.

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
| GET / POST | `/1/keys` | admin |
| GET / PUT / DELETE | `/1/keys/{key}` | admin |
| GET | `/1/isalive` | none |

See [ROADMAP.md](ROADMAP.md) for the plan and open items.

Datagen flags: `--products N --movies N --articles N --seed S`.
