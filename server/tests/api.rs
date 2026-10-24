//! End-to-end tests of the HTTP API, without opening a socket.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use server::{AppState, app};
use std::sync::Arc;
use tower::ServiceExt;

const ADMIN: &str = "admin-key";
const SEARCH: &str = "search-key";

struct Client {
    router: axum::Router,
}

impl Client {
    fn new() -> Self {
        Client { router: app(Arc::new(AppState::new(ADMIN.into(), SEARCH.into()))) }
    }

    /// A server backed by `dir`; a new one on the same dir is a restart.
    fn persistent(dir: &std::path::Path) -> Self {
        Client { router: app(Arc::new(AppState::open(ADMIN.into(), SEARCH.into(), dir).unwrap())) }
    }

    async fn call(&self, method: Method, path: &str, key: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let (status, json, _) = self.call_from(method, path, key, body, None, &[]).await;
        (status, json)
    }

    /// A request from client address `ip`, with extra headers; also returns
    /// the response headers.
    async fn call_from(
        &self,
        method: Method,
        path: &str,
        key: Option<&str>,
        body: Option<Value>,
        ip: Option<&str>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value, axum::http::HeaderMap) {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(key) = key {
            request = request.header("X-Algolia-API-Key", key);
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let body = body.map_or(Body::empty(), |b| Body::from(b.to_string()));
        let mut request = request.body(body).unwrap();
        if let Some(ip) = ip {
            let addr: std::net::SocketAddr = format!("{ip}:5000").parse().unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(addr));
        }
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
        (status, json, headers)
    }

    async fn admin(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let (status, json) = self.call(method, path, Some(ADMIN), body).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        json
    }

    async fn search(&self, index: &str, body: Value) -> Value {
        let (status, json) = self.call(Method::POST, &format!("/1/indexes/{index}/query"), Some(SEARCH), Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        json
    }

    async fn seed(&self) {
        self.admin(
            Method::PUT,
            "/1/indexes/products/settings",
            Some(json!({
                "searchableAttributes": ["name", "brand"],
                "attributesForFaceting": ["brand"],
                "customRanking": ["desc(rating)"]
            })),
        )
        .await;
        self.admin(
            Method::POST,
            "/1/indexes/products/batch",
            Some(json!({"requests": [
                {"action": "addObject", "body": {"objectID": "1", "name": "Wireless Headphones", "brand": "Élan", "price": 99, "rating": 4.5}},
                {"action": "addObject", "body": {"objectID": "2", "name": "Wireless Mouse", "brand": "Orbix", "price": 25, "rating": 4.0}},
                {"action": "addObject", "body": {"objectID": "3", "name": "Kettle", "brand": "Élan", "price": 40, "rating": 3.0}}
            ]})),
        )
        .await;
    }
}

fn ids(response: &Value) -> Vec<&str> {
    response["hits"].as_array().unwrap().iter().map(|h| h["objectID"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn search_returns_algolia_shaped_results() {
    let c = Client::new();
    c.seed().await;
    let r = c.search("products", json!({"query": "wirless head", "getRankingInfo": true, "facets": ["brand"]})).await;
    assert_eq!(ids(&r), ["1"]);
    assert_eq!((r["nbHits"].as_u64(), r["page"].as_u64(), r["hitsPerPage"].as_u64()), (Some(1), Some(0), Some(20)));
    assert_eq!(r["query"], "wirless head");
    assert_eq!(r["index"], "products");
    let hit = &r["hits"][0];
    assert_eq!(hit["_highlightResult"]["name"]["value"], "<em>Wireless</em> <em>Head</em>phones");
    assert_eq!(hit["_rankingInfo"]["nbTypos"], 1);
    assert_eq!(r["facets"]["brand"], json!({"Élan": 1}));
    assert!(r["processingTimeMS"].is_u64());
}

#[tokio::test]
async fn params_string_get_search_and_multi_query() {
    let c = Client::new();
    c.seed().await;
    let r = c.search("products", json!({"params": "query=wireless&filters=price%20%3C%2050&attributesToRetrieve=name"})).await;
    assert_eq!(ids(&r), ["2"]);
    assert_eq!(r["hits"][0].as_object().unwrap().keys().filter(|k| !k.starts_with('_')).count(), 2);
    let highlighted: Vec<&String> = r["hits"][0]["_highlightResult"].as_object().unwrap().keys().collect();
    assert_eq!(highlighted, ["name"]);

    let (status, r) = c.call(Method::GET, "/1/indexes/products?query=kettle", Some(SEARCH), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&r), ["3"]);

    let (status, r) = c
        .call(
            Method::POST,
            "/1/indexes/*/queries",
            Some(SEARCH),
            Some(json!({"requests": [
                {"indexName": "products", "params": "query=wireless"},
                {"indexName": "products", "query": "", "filters": "brand:elan"}
            ]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(ids(&r["results"][0]), ["1", "2"]);
    assert_eq!(ids(&r["results"][1]), ["1", "3"]);
}

#[tokio::test]
async fn keys_are_enforced() {
    let c = Client::new();
    c.seed().await;
    let (status, r) = c.call(Method::POST, "/1/indexes/products/query", None, Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(r["status"], 403);
    let (status, _) = c.call(Method::POST, "/1/indexes/products/query", Some("wrong"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // The search key cannot write or read settings.
    let (status, _) = c.call(Method::PUT, "/1/indexes/products/9", Some(SEARCH), Some(json!({"name": "x"}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = c.call(Method::GET, "/1/indexes/products/settings", Some(SEARCH), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // The key may also be a query parameter, as with Algolia.
    let (status, _) = c.call(Method::GET, "/1/indexes/products?query=a&x-algolia-api-key=search-key", None, None).await;
    assert_eq!(status, StatusCode::OK);
    // The admin key can search too.
    let (status, _) = c.call(Method::POST, "/1/indexes/products/query", Some(ADMIN), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = c.call(Method::GET, "/1/isalive", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn object_crud_and_partial_updates() {
    let c = Client::new();
    c.seed().await;
    let created = c.admin(Method::POST, "/1/indexes/products", Some(json!({"name": "Toaster"}))).await;
    let new_id = created["objectID"].as_str().unwrap().to_owned();
    assert!(created["taskID"].is_u64());
    assert_eq!(created["createdAt"].as_str().unwrap().len(), "2026-10-23T06:15:00Z".len());

    let r = c.admin(Method::GET, &format!("/1/indexes/products/{new_id}"), None).await;
    assert_eq!(r["name"], "Toaster");

    c.admin(Method::PUT, "/1/indexes/products/3", Some(json!({"name": "Teapot", "rating": 1}))).await;
    assert!(ids(&c.search("products", json!({"query": "kettle"})).await).is_empty());
    assert_eq!(ids(&c.search("products", json!({"query": "teapot"})).await), ["3"]);

    c.admin(Method::POST, "/1/indexes/products/3/partial", Some(json!({"brand": "Orbix"}))).await;
    let r = c.admin(Method::GET, "/1/indexes/products/3", None).await;
    assert_eq!((r["name"].as_str(), r["brand"].as_str()), (Some("Teapot"), Some("Orbix")));

    c.admin(Method::POST, "/1/indexes/products/99/partial?createIfNotExists=false", Some(json!({"a": 1}))).await;
    let (status, _) = c.call(Method::GET, "/1/indexes/products/99", Some(ADMIN), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    c.admin(Method::DELETE, "/1/indexes/products/1", None).await;
    assert_eq!(ids(&c.search("products", json!({"query": "wireless"})).await), ["2"]);
}

#[tokio::test]
async fn batch_is_validated_before_writing() {
    let c = Client::new();
    c.seed().await;
    let (status, r) = c
        .call(
            Method::POST,
            "/1/indexes/products/batch",
            Some(ADMIN),
            Some(json!({"requests": [
                {"action": "deleteObject", "body": {"objectID": "1"}},
                {"action": "fly", "body": {}}
            ]})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(r["message"], "requests[1]: unknown action \"fly\"");
    assert_eq!(c.search("products", json!({})).await["nbHits"], 3);

    let r = c
        .admin(
            Method::POST,
            "/1/indexes/products/batch",
            Some(json!({"requests": [
                {"action": "partialUpdateObject", "body": {"objectID": "2", "price": 5}},
                {"action": "deleteObject", "body": {"objectID": "1"}},
                {"action": "partialUpdateObjectNoCreate", "body": {"objectID": "77", "price": 5}}
            ]})),
        )
        .await;
    assert_eq!(r["objectIDs"], json!(["2", "1", "77"]));
    assert_eq!(ids(&c.search("products", json!({"filters": "price < 10"})).await), ["2"]);
    let (status, _) = c.call(Method::GET, "/1/indexes/products/77", Some(ADMIN), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn settings_merge_and_reindex() {
    let c = Client::new();
    c.seed().await;
    assert_eq!(ids(&c.search("products", json!({"query": "orbix"})).await), ["2"]);
    // Changing one setting keeps the others and re-indexes the records.
    c.admin(Method::PUT, "/1/indexes/products/settings", Some(json!({"searchableAttributes": ["name"]}))).await;
    assert!(ids(&c.search("products", json!({"query": "orbix"})).await).is_empty());
    let settings = c.admin(Method::GET, "/1/indexes/products/settings", None).await;
    assert_eq!(settings["searchableAttributes"], json!(["name"]));
    assert_eq!(settings["customRanking"], json!(["desc(rating)"]));
    assert_eq!(settings["attributesForFaceting"], json!(["brand"]));

    let (status, r) =
        c.call(Method::PUT, "/1/indexes/products/settings", Some(ADMIN), Some(json!({"customRanking": ["rating"]}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(r["message"].as_str().unwrap().starts_with("invalid settings"));
}

#[tokio::test]
async fn index_management_and_errors() {
    let c = Client::new();
    c.seed().await;
    let list = c.admin(Method::GET, "/1/indexes", None).await;
    assert_eq!(list["items"], json!([{"name": "products", "entries": 3}]));

    let (status, r) = c.call(Method::POST, "/1/indexes/missing/query", Some(SEARCH), Some(json!({}))).await;
    assert_eq!((status, r["message"].as_str()), (StatusCode::NOT_FOUND, Some("Index missing does not exist")));

    let (status, r) = c.call(Method::POST, "/1/indexes/products/query", Some(SEARCH), Some(json!({"filters": "name:x"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(r["message"], "invalid filter: 'name' is not in attributesForFaceting");

    let (status, _) = c.call(Method::POST, "/1/indexes/bad%20name", Some(ADMIN), Some(json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = c.call(Method::POST, "/1/indexes/products/query", Some(SEARCH), Some(json!([1]))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    c.admin(Method::POST, "/1/indexes/products/clear", None).await;
    assert_eq!(c.search("products", json!({})).await["nbHits"], 0);
    let task = c.admin(Method::GET, "/1/indexes/products/task/1", None).await;
    assert_eq!(task["status"], "published");
    c.admin(Method::DELETE, "/1/indexes/products", None).await;
    let (status, _) = c.call(Method::POST, "/1/indexes/products/query", Some(SEARCH), Some(json!({}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn data_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let c = Client::persistent(dir.path());
        c.seed().await;
        c.admin(Method::PUT, "/1/indexes/products/3", Some(json!({"name": "Teapot", "brand": "Élan", "rating": 5}))).await;
        c.admin(Method::DELETE, "/1/indexes/products/2", None).await;
        c.admin(Method::POST, "/1/indexes/notes", Some(json!({"objectID": "n1", "text": "remember the milk"}))).await;
    }
    let c = Client::persistent(dir.path());
    let r = c.search("products", json!({"query": "", "facets": ["brand"]})).await;
    assert_eq!(ids(&r), ["3", "1"]);
    assert_eq!(r["facets"]["brand"], json!({"Élan": 2}));
    // Settings came back too: brand is still searchable, ranking is by rating.
    let settings = c.admin(Method::GET, "/1/indexes/products/settings", None).await;
    assert_eq!(settings["customRanking"], json!(["desc(rating)"]));
    assert_eq!(ids(&c.search("notes", json!({"query": "milk"})).await), ["n1"]);

    // Deleting an index removes it from disk as well.
    c.admin(Method::DELETE, "/1/indexes/notes", None).await;
    let c = Client::persistent(dir.path());
    let list = c.admin(Method::GET, "/1/indexes", None).await;
    assert_eq!(list["items"], json!([{"name": "products", "entries": 2}]));
}

#[tokio::test]
async fn batch_partial_updates_see_earlier_operations() {
    let dir = tempfile::tempdir().unwrap();
    {
        let c = Client::persistent(dir.path());
        c.admin(
            Method::POST,
            "/1/indexes/items/batch",
            Some(json!({"requests": [
                {"action": "addObject", "body": {"objectID": "a", "name": "Lamp", "price": 10}},
                {"action": "partialUpdateObject", "body": {"objectID": "a", "price": 12}},
                {"action": "clear"},
                {"action": "partialUpdateObjectNoCreate", "body": {"objectID": "a", "price": 99}},
                {"action": "addObject", "body": {"objectID": "b", "name": "Desk"}},
                {"action": "partialUpdateObject", "body": {"objectID": "b", "color": "oak"}}
            ]})),
        )
        .await;
    }
    let c = Client::persistent(dir.path());
    let (status, _) = c.call(Method::GET, "/1/indexes/items/a", Some(ADMIN), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let b = c.admin(Method::GET, "/1/indexes/items/b", None).await;
    assert_eq!(b, json!({"objectID": "b", "name": "Desk", "color": "oak"}));
}

#[tokio::test]
async fn index_names_cannot_escape_the_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let c = Client::persistent(dir.path());
    for name in ["..", ".hidden", "."] {
        let (status, _) = c.call(Method::POST, &format!("/1/indexes/{name}/batch"), Some(ADMIN), Some(json!({"requests": []}))).await;
        assert_ne!(status, StatusCode::OK, "{name}");
    }
    assert!(std::fs::read_dir(dir.path().join("indexes")).unwrap().next().is_none());
}

#[tokio::test]
async fn query_understanding_over_http() {
    let c = Client::new();
    c.seed().await;
    c.admin(
        Method::PUT,
        "/1/indexes/products/settings",
        Some(json!({"ignorePlurals": true, "synonyms": [{"type": "synonym", "synonyms": ["earphones", "headphones"]}]})),
    )
    .await;
    assert_eq!(ids(&c.search("products", json!({"query": "earphones"})).await), ["1"]);
    assert_eq!(ids(&c.search("products", json!({"query": "kettles"})).await), ["3"]);
    let r = c.search("products", json!({"params": "query=wireless%20mouse%20pad&removeWordsIfNoResults=lastWords"})).await;
    assert_eq!(ids(&r), ["2"]);
    assert_eq!(r["queryAfterRemoval"], "wireless mouse <em>pad</em>");
    let r = c.search("products", json!({"query": "wireless kettle", "optionalWords": "wireless"})).await;
    assert_eq!(ids(&r), ["3"]);
    assert!(r.get("queryAfterRemoval").is_none());
}

#[cfg(feature = "japanese")]
#[tokio::test]
async fn japanese_index_over_http_and_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let client = Client::persistent(dir.path());
    client
        .admin(Method::PUT, "/1/indexes/shop/settings", Some(json!({"searchableAttributes": ["name"], "indexLanguages": ["ja"]})))
        .await;
    client.admin(Method::PUT, "/1/indexes/shop/1", Some(json!({"name": "関西国際空港限定トートバッグ"}))).await;
    client.admin(Method::PUT, "/1/indexes/shop/2", Some(json!({"name": "電子辞書"}))).await;
    let settings = client.admin(Method::GET, "/1/indexes/shop/settings", None).await;
    assert_eq!(settings["indexLanguages"], json!(["ja"]));
    drop(client);

    let client = Client::persistent(dir.path());
    let response = client.search("shop", json!({"query": "空港"})).await;
    assert_eq!(ids(&response), ["1"]);
    assert_eq!(response["hits"][0]["_highlightResult"]["name"]["value"], "関西国際<em>空港</em>限定トートバッグ");
    // Kanji only, split as Japanese (電子 + 辞書) because of indexLanguages.
    assert_eq!(ids(&client.search("shop", json!({"query": "辞書"})).await), ["2"]);
}


/// Creates a key and returns its value.
async fn create_key(client: &Client, definition: Value) -> String {
    let created = client.admin(Method::POST, "/1/keys", Some(definition)).await;
    created["key"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn keys_are_created_listed_updated_and_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let client = Client::persistent(dir.path());
    let value = create_key(&client, json!({"acl": ["search"], "description": "web", "indexes": ["dev_*"]})).await;
    assert_eq!(value.len(), 32);
    let key = client.admin(Method::GET, &format!("/1/keys/{value}"), None).await;
    assert_eq!((key["acl"].clone(), key["indexes"].clone()), (json!(["search"]), json!(["dev_*"])));
    client.admin(Method::PUT, &format!("/1/keys/{value}"), Some(json!({"maxHitsPerQuery": 5}))).await;
    drop(client);

    // Keys survive a restart.
    let client = Client::persistent(dir.path());
    let listed = client.admin(Method::GET, "/1/keys", None).await;
    assert_eq!(listed["keys"].as_array().unwrap().len(), 1);
    assert_eq!(listed["keys"][0]["maxHitsPerQuery"], 5);
    assert_eq!(listed["keys"][0]["description"], "web");

    // Only the admin key manages keys, and bad definitions are refused.
    let (status, _) = client.call(Method::GET, "/1/keys", Some(SEARCH), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = client.call(Method::GET, "/1/keys", Some(&value), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for bad in [json!({"acl": ["serch"]}), json!({"acl": []}), json!({"acl": ["search"], "colour": 1})] {
        let (status, _) = client.call(Method::POST, "/1/keys", Some(ADMIN), Some(bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    client.admin(Method::DELETE, &format!("/1/keys/{value}"), None).await;
    let (status, _) = client.call(Method::GET, &format!("/1/keys/{value}"), Some(ADMIN), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = client.call(Method::POST, "/1/indexes/dev_a/query", Some(&value), Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn key_rights_and_index_patterns_are_enforced() {
    let client = Client::new();
    client.seed().await;
    let writer = create_key(&client, json!({"acl": ["addObject"], "indexes": ["dev_*"]})).await;
    let put = |index: &str| format!("/1/indexes/{index}/1");
    let (status, _) = client.call(Method::PUT, &put("dev_products"), Some(&writer), Some(json!({"name": "x"}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, json) = client.call(Method::PUT, &put("products"), Some(&writer), Some(json!({"name": "x"}))).await;
    assert_eq!((status, json["message"].as_str().unwrap()), (StatusCode::FORBIDDEN, "Index not allowed with this API key"));
    // No search, delete or settings rights.
    for (method, path) in [
        (Method::POST, "/1/indexes/dev_products/query"),
        (Method::DELETE, "/1/indexes/dev_products/1"),
        (Method::DELETE, "/1/indexes/dev_products"),
        (Method::PUT, "/1/indexes/dev_products/settings"),
        (Method::GET, "/1/indexes"),
    ] {
        let (status, _) = client.call(method.clone(), path, Some(&writer), Some(json!({}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }
    // Batches are checked action by action.
    let batch = json!({"requests": [{"action": "addObject", "body": {"name": "y"}}, {"action": "deleteObject", "body": {"objectID": "1"}}]});
    let (status, _) = client.call(Method::POST, "/1/indexes/dev_products/batch", Some(&writer), Some(batch)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(client.admin(Method::GET, "/1/indexes/dev_products/1", None).await["name"], "x");

    // Listing shows only the allowed indexes.
    let lister = create_key(&client, json!({"acl": ["listIndexes"], "indexes": ["dev_*"]})).await;
    let (_, listing) = client.call(Method::GET, "/1/indexes", Some(&lister), None).await;
    let names: Vec<&str> = listing["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["dev_products"]);
}

#[tokio::test]
async fn secured_keys_restrict_searches() {
    let client = Client::new();
    client.seed().await;
    let secured = server::keys::generate_secured_key(SEARCH, "filters=brand%3A%C3%89lan&restrictIndices=products&hitsPerPage=1");
    let search = |body: Value| client.call(Method::POST, "/1/indexes/products/query", Some(&secured), Some(body));

    let (status, response) = search(json!({"query": ""})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["nbHits"], 2); // the two Élan records
    assert_eq!(response["hitsPerPage"], 1); // forced by the key
    // The request's filters narrow further but cannot widen.
    let (_, response) = search(json!({"filters": "brand:Orbix"})).await;
    assert_eq!(response["nbHits"], 0);
    let (_, response) = search(json!({"filters": "brand:Orbix OR brand:Élan", "hitsPerPage": 50})).await;
    assert_eq!((response["nbHits"].clone(), response["hitsPerPage"].clone()), (json!(2), json!(1)));
    let (status, _) = search(json!({"filters": "brand:x) OR (brand:Orbix"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Other indexes, records by objectID, and writes are refused.
    let (status, _) = client.call(Method::POST, "/1/indexes/other/query", Some(&secured), Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = client.call(Method::GET, "/1/indexes/products/2", Some(&secured), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = client.call(Method::PUT, "/1/indexes/products/9", Some(&secured), Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Multi-queries are checked per index.
    let multi = json!({"requests": [{"indexName": "products"}, {"indexName": "other"}]});
    let (status, _) = client.call(Method::POST, "/1/indexes/*/queries", Some(&secured), Some(multi)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Expired, or signed with something other than a search key.
    let expired = server::keys::generate_secured_key(SEARCH, "validUntil=1000");
    let (status, json) = client.call(Method::POST, "/1/indexes/products/query", Some(&expired), Some(json!({}))).await;
    assert_eq!((status, json["message"].as_str().unwrap()), (StatusCode::FORBIDDEN, "The API key has expired"));
    let forged = server::keys::generate_secured_key(ADMIN, "filters=brand%3AOrbix");
    let (status, _) = client.call(Method::POST, "/1/indexes/products/query", Some(&forged), Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn secured_keys_inherit_a_created_parents_limits() {
    let client = Client::new();
    client.seed().await;
    let parent = create_key(&client, json!({"acl": ["search", "addObject"], "indexes": ["products"], "queryParameters": "filters=price%20%3C%2050"})).await;
    let secured = server::keys::generate_secured_key(&parent, "filters=brand%3A%C3%89lan");
    let (status, response) =
        client.call(Method::POST, "/1/indexes/products/query", Some(&secured), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    // Both filters apply: Élan and under 50 is only the kettle.
    assert_eq!(ids(&response), ["3"]);
    // The parent may write; the secured key may not.
    let (status, _) = client.call(Method::PUT, "/1/indexes/products/9", Some(&secured), Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Deleting the parent invalidates its secured keys.
    client.admin(Method::DELETE, &format!("/1/keys/{parent}"), None).await;
    let (status, _) = client.call(Method::POST, "/1/indexes/products/query", Some(&secured), Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rate_limits_per_ip_and_per_user_token() {
    let client = Client::new();
    client.seed().await;
    let limited = create_key(&client, json!({"acl": ["search"], "maxQueriesPerIPPerHour": 3})).await;
    let client = &client;
    let query = |key: String, ip: &'static str| async move {
        let path = "/1/indexes/products/query";
        client.call_from(Method::POST, path, Some(&key), Some(json!({})), Some(ip), &[]).await
    };
    for _ in 0..3 {
        assert_eq!(query(limited.clone(), "10.0.0.1").await.0, StatusCode::OK);
    }
    let (status, json, headers) = query(limited.clone(), "10.0.0.1").await;
    assert_eq!((status, json["message"].as_str().unwrap()), (StatusCode::TOO_MANY_REQUESTS, "Too many requests"));
    let wait: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=3600).contains(&wait));
    // Another address has its own count.
    assert_eq!(query(limited.clone(), "10.0.0.2").await.0, StatusCode::OK);
    // Secured keys with a userToken are counted per user, even from one address.
    for user in ["alice", "bob"] {
        let secured = server::keys::generate_secured_key(&limited, &format!("userToken={user}"));
        for _ in 0..3 {
            assert_eq!(query(secured.clone(), "10.0.0.1").await.0, StatusCode::OK);
        }
        assert_eq!(query(secured, "10.0.0.1").await.0, StatusCode::TOO_MANY_REQUESTS);
    }
    // Keys without a limit are not limited.
    for _ in 0..10 {
        assert_eq!(query(SEARCH.to_owned(), "10.0.0.1").await.0, StatusCode::OK);
    }
}

#[tokio::test]
async fn guessing_keys_is_slowed_down() {
    let client = Client::new();
    client.seed().await;
    let path = "/1/indexes/products/query";
    for i in 0..server::MAX_FAILED_AUTH_PER_HOUR {
        let key = format!("guess-{i}");
        let (status, _, _) = client.call_from(Method::POST, path, Some(&key), Some(json!({})), Some("10.9.9.9"), &[]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    // Now even a correct key from that address waits; others are unaffected.
    let (status, _, _) = client.call_from(Method::POST, path, Some(SEARCH), Some(json!({})), Some("10.9.9.9"), &[]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let (status, _, _) = client.call_from(Method::POST, path, Some(SEARCH), Some(json!({})), Some("10.9.9.8"), &[]).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn sources_and_referers_restrict_keys() {
    let client = Client::new();
    client.seed().await;
    let path = "/1/indexes/products/query";
    let office = server::keys::generate_secured_key(SEARCH, "restrictSources=192.168.0.0%2F16");
    let from = |ip| client.call_from(Method::POST, path, Some(&office), Some(json!({})), Some(ip), &[]);
    assert_eq!(from("192.168.4.4").await.0, StatusCode::OK);
    assert_eq!(from("8.8.8.8").await.0, StatusCode::FORBIDDEN);

    let web = create_key(&client, json!({"acl": ["search"], "referers": ["https://shop.example.com/*"]})).await;
    let client = &client;
    let with = |headers: &'static [(&'static str, &'static str)]| {
        let web = web.clone();
        async move { client.call_from(Method::POST, path, Some(&web), Some(json!({})), None, headers).await.0 }
    };
    assert_eq!(with(&[("referer", "https://shop.example.com/cart")]).await, StatusCode::OK);
    assert_eq!(with(&[("origin", "https://shop.example.com/")]).await, StatusCode::OK);
    assert_eq!(with(&[("referer", "https://other.example.com/")]).await, StatusCode::FORBIDDEN);
    assert_eq!(with(&[]).await, StatusCode::FORBIDDEN);
}
