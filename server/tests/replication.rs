//! Primary and replica servers on real sockets, in one process.

use serde_json::{Value, json};
use server::replication::run_replica;
use server::{AppState, app};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

const ADMIN: &str = "admin-key";
const SEARCH: &str = "search-key";

/// A server with its own runtime on its own thread, so killing it closes
/// every socket at once, like a process dying.
struct Node {
    url: String,
    state: Arc<AppState>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Node {
    fn start(state: Arc<AppState>, port: u16, replicate: bool) -> Node {
        let (url_tx, url_rx) = std::sync::mpsc::channel();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let node_state = state.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            runtime.block_on(async move {
                let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
                url_tx.send(format!("http://{}", listener.local_addr().unwrap())).unwrap();
                if replicate {
                    tokio::spawn(run_replica(node_state.clone(), ADMIN.into()));
                }
                tokio::select! {
                    _ = axum::serve(listener, app(node_state)) => {}
                    _ = stopped => {}
                }
            });
            // Dropping the runtime here cancels every connection and task.
        });
        let url = url_rx.recv().unwrap();
        Node { url, state, stop: Some(stop), thread: Some(thread) }
    }

    /// Stops the node at once, as if the process died.
    fn kill(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

async fn primary(dir: &Path, recent_limit: usize, port: u16) -> Node {
    let state = Arc::new(AppState::open(ADMIN.into(), SEARCH.into(), dir).unwrap().with_recent_limit(recent_limit));
    Node::start(state, port, false)
}

async fn replica(dir: &Path, primary_url: &str) -> Node {
    let state = Arc::new(AppState::open(ADMIN.into(), SEARCH.into(), dir).unwrap().as_replica_of(primary_url));
    Node::start(state, 0, true)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn call(method: reqwest::Method, url: String, key: &str, body: Option<Value>) -> (u16, Value) {
    let mut request = client().request(method, url).header("X-Algolia-API-Key", key);
    if let Some(body) = body {
        request = request.body(body.to_string());
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

async fn write(node: &Node, method: reqwest::Method, path: &str, body: Value) {
    let (status, r) = call(method, format!("{}{path}", node.url), ADMIN, Some(body)).await;
    assert_eq!(status, 200, "{r}");
}

/// objectIDs found by a search, or None if the index does not exist there.
async fn search(node: &Node, index: &str, body: Value) -> Option<Vec<String>> {
    let (status, r) = call(reqwest::Method::POST, format!("{}/1/indexes/{index}/query", node.url), SEARCH, Some(body)).await;
    if status == 404 {
        return None;
    }
    assert_eq!(status, 200, "{r}");
    let mut ids: Vec<String> =
        r["hits"].as_array().unwrap().iter().map(|h| h["objectID"].as_str().unwrap().to_owned()).collect();
    ids.sort();
    Some(ids)
}

/// Retries `check` until it returns true (replication is asynchronous).
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    while !check().await {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn has(node: &Node, index: &str, query: &str, expected: &[&str]) -> bool {
    search(node, index, json!({"query": query, "hitsPerPage": 100})).await.is_some_and(|ids| ids == expected)
}

async fn seed(p: &Node) {
    use reqwest::Method;
    write(p, Method::PUT, "/1/indexes/products/settings", json!({
        "searchableAttributes": ["name"], "attributesForFaceting": ["brand"], "customRanking": ["desc(rating)"]
    }))
    .await;
    write(p, Method::POST, "/1/indexes/products/batch", json!({"requests": [
        {"action": "addObject", "body": {"objectID": "1", "name": "Wireless Headphones", "brand": "Élan", "rating": 4}},
        {"action": "addObject", "body": {"objectID": "2", "name": "Wireless Mouse", "brand": "Orbix", "rating": 5}},
        {"action": "addObject", "body": {"objectID": "3", "name": "Kettle", "brand": "Élan", "rating": 3}}
    ]}))
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn replica_copies_data_and_follows_every_kind_of_change() {
    use reqwest::Method;
    let (pd, rd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let p = primary(pd.path(), 1000, 0).await;
    seed(&p).await;
    let r = replica(rd.path(), &p.url).await;
    eventually("initial copy", || has(&r, "products", "wireless", &["1", "2"])).await;

    write(&p, Method::PUT, "/1/indexes/products/4", json!({"name": "Wireless Charger", "brand": "Orbix", "rating": 1})).await;
    write(&p, Method::DELETE, "/1/indexes/products/1", json!({})).await;
    write(&p, Method::POST, "/1/indexes/products/3/partial", json!({"name": "Electric Kettle"})).await;
    // Each check waits: a replica may briefly show the first changes
    // without the later ones, always in the primary's order.
    eventually("put and delete", || has(&r, "products", "wireless", &["2", "4"])).await;
    eventually("partial update", || has(&r, "products", "electric", &["3"])).await;

    // Settings replicate too: after this, brand is searchable on the replica.
    write(&p, Method::PUT, "/1/indexes/products/settings", json!({"searchableAttributes": ["name", "brand"]})).await;
    eventually("settings change", || has(&r, "products", "orbix", &["2", "4"])).await;
    let (_, settings) = call(Method::GET, format!("{}/1/indexes/products/settings", r.url), ADMIN, None).await;
    assert_eq!(settings["customRanking"], json!(["desc(rating)"]));

    // Same sequence number on both sides, and the replica reports no lag.
    let (_, status) = call(Method::GET, format!("{}/1/replication/status", r.url), ADMIN, None).await;
    assert_eq!(status["role"], "replica");
    let seq = p.state.index("products").unwrap().position().0;
    assert_eq!(status["indexes"][0]["seq"], seq);
    assert_eq!(status["indexes"][0]["lag"], 0);
    assert!(status["lastError"].is_null());

    write(&p, Method::POST, "/1/indexes/products/clear", json!({})).await;
    eventually("clear", || has(&r, "products", "", &[])).await;
    p.kill();
    r.kill();
}

#[tokio::test(flavor = "multi_thread")]
async fn replica_refuses_writes_but_serves_searches() {
    use reqwest::Method;
    let (pd, rd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let p = primary(pd.path(), 1000, 0).await;
    seed(&p).await;
    let r = replica(rd.path(), &p.url).await;
    eventually("initial copy", || has(&r, "products", "kettle", &["3"])).await;

    for (method, path) in [
        (Method::PUT, "/1/indexes/products/9"),
        (Method::POST, "/1/indexes/products/batch"),
        (Method::PUT, "/1/indexes/products/settings"),
        (Method::POST, "/1/indexes/newindex"),
    ] {
        let (status, body) = call(method, format!("{}{path}", r.url), ADMIN, Some(json!({"requests": []}))).await;
        assert_eq!(status, 403, "{path}");
        assert!(body["message"].as_str().unwrap().contains("read replica"), "{body}");
    }
    let (status, _) = call(Method::DELETE, format!("{}/1/indexes/products", r.url), ADMIN, None).await;
    assert_eq!(status, 403);
    // Reads with the admin key still work on a replica.
    let (status, record) = call(Method::GET, format!("{}/1/indexes/products/3", r.url), ADMIN, None).await;
    assert_eq!((status, record["name"].as_str()), (200, Some("Kettle")));
    p.kill();
    r.kill();
}

#[tokio::test(flavor = "multi_thread")]
async fn deleted_and_recreated_indexes_follow_the_primary() {
    use reqwest::Method;
    let (pd, rd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let p = primary(pd.path(), 1000, 0).await;
    seed(&p).await;
    write(&p, Method::POST, "/1/indexes/notes", json!({"objectID": "n1", "text": "old note"})).await;
    let r = replica(rd.path(), &p.url).await;
    eventually("both indexes", || has(&r, "notes", "old", &["n1"])).await;

    write(&p, Method::DELETE, "/1/indexes/notes", json!({})).await;
    eventually("deletion", || async { search(&r, "notes", json!({})).await.is_none() }).await;
    // The files go right after the index leaves memory.
    eventually("files removed", || async { !rd.path().join("indexes/notes").exists() }).await;

    // Re-created with the same name: a new epoch, so the replica starts over.
    write(&p, Method::POST, "/1/indexes/notes", json!({"objectID": "n2", "text": "new note"})).await;
    eventually("re-creation", || has(&r, "notes", "note", &["n2"])).await;
    p.kill();
    r.kill();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_replica_catches_up_from_the_log_or_a_snapshot() {
    use reqwest::Method;
    let (pd, rd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // The primary keeps only 5 changes in memory.
    let p = primary(pd.path(), 5, 0).await;
    seed(&p).await;
    let r = replica(rd.path(), &p.url).await;
    eventually("initial copy", || has(&r, "products", "kettle", &["3"])).await;
    r.kill();

    // A few changes: still in the primary's memory, so the log is enough.
    write(&p, Method::PUT, "/1/indexes/products/5", json!({"name": "Desk Lamp"})).await;
    let r = replica(rd.path(), &p.url).await;
    assert!(has(&r, "products", "kettle", &["3"]).await, "the replica serves its own copy right after restart");
    eventually("resume from log", || has(&r, "products", "lamp", &["5"])).await;
    r.kill();

    // Many changes: more than the primary keeps, so a snapshot is needed.
    for i in 0..20 {
        write(&p, Method::PUT, &format!("/1/indexes/products/bulk{i}"), json!({"name": format!("Bulk Lamp {i}")})).await;
    }
    let r = replica(rd.path(), &p.url).await;
    eventually("catch up by snapshot", || async {
        search(&r, "products", json!({"query": "lamp", "hitsPerPage": 100})).await.is_some_and(|ids| ids.len() == 21)
    })
    .await;
    assert_eq!(r.state.index("products").unwrap().position(), p.state.index("products").unwrap().position());
    p.kill();
    r.kill();
}

#[tokio::test(flavor = "multi_thread")]
async fn replica_keeps_serving_while_the_primary_is_down() {
    use reqwest::Method;
    let (pd, rd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let p = primary(pd.path(), 1000, 0).await;
    let port: u16 = p.url.rsplit(':').next().unwrap().parse().unwrap();
    seed(&p).await;
    let r = replica(rd.path(), &p.url).await;
    eventually("initial copy", || has(&r, "products", "wireless", &["1", "2"])).await;

    p.kill();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(has(&r, "products", "wireless", &["1", "2"]).await, "searches keep working");
    eventually("error is reported", || async {
        let (_, status) = call(Method::GET, format!("{}/1/replication/status", r.url), ADMIN, None).await;
        status["lastError"].is_string()
    })
    .await;

    // The primary comes back from its data directory and takes a write.
    let p = primary(pd.path(), 1000, port).await;
    write(&p, Method::PUT, "/1/indexes/products/7", json!({"name": "Wireless Speaker", "rating": 2})).await;
    eventually("catch up after outage", || has(&r, "products", "wireless", &["1", "2", "7"])).await;
    eventually("error cleared", || async {
        let (_, status) = call(Method::GET, format!("{}/1/replication/status", r.url), ADMIN, None).await;
        status["lastError"].is_null()
    })
    .await;
    p.kill();
    r.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_keys_reach_replicas() {
    let (pdir, rdir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let p = primary(pdir.path(), 1000, 0).await;
    seed(&p).await;
    let r = replica(rdir.path(), &p.url).await;
    let (status, created) = call(reqwest::Method::POST, format!("{}/1/keys", p.url), ADMIN, Some(json!({"acl": ["search"]}))).await;
    assert_eq!(status, 200, "{created}");
    let key = created["key"].as_str().unwrap().to_owned();
    let secured = server::keys::generate_secured_key(&key, "filters=brand%3AOrbix");
    let status_on = |node: &Node, key: String| {
        let url = format!("{}/1/indexes/products/query", node.url);
        async move { call(reqwest::Method::POST, url, &key, Some(json!({}))).await.0 }
    };
    eventually("the key works on the replica", || async { status_on(&r, key.clone()).await == 200 }).await;
    assert_eq!(status_on(&r, secured.clone()).await, 200);
    // Key changes are refused on a replica.
    let (status, _) = call(reqwest::Method::POST, format!("{}/1/keys", r.url), ADMIN, Some(json!({"acl": ["search"]}))).await;
    assert_eq!(status, 403);
    // A deleted key stops working on the replica too.
    let (status, _) = call(reqwest::Method::DELETE, format!("{}/1/keys/{key}", p.url), ADMIN, None).await;
    assert_eq!(status, 200);
    eventually("the key is gone from the replica", || async { status_on(&r, key.clone()).await == 403 }).await;
    assert_eq!(status_on(&r, secured).await, 403);
    r.kill();
    p.kill();
}
