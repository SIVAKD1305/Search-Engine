//! Raft clusters of real servers on real sockets, in one process. Each node
//! has its own runtime on its own thread, so killing one closes every socket
//! at once, as when a process dies; its data directory stays for a restart.

use serde_json::{Value, json};
use server::cluster::{Cluster, ClusterConfig, Member};
use server::{AppState, app};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ADMIN: &str = "admin-key";
const SEARCH: &str = "search-key";

struct Node {
    id: String,
    url: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Node {
    fn start(id: &str, addr: SocketAddr, members: Vec<Member>, dir: &Path, snapshot_every: u64) -> Node {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (id_owned, dir) = (id.to_owned(), dir.to_owned());
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            runtime.block_on(async move {
                let socket = tokio::net::TcpSocket::new_v4().unwrap();
                socket.set_reuseaddr(true).unwrap();
                socket.bind(addr).unwrap();
                let listener = socket.listen(128).unwrap();
                let state = Arc::new(AppState::new(ADMIN.into(), SEARCH.into()));
                let mut config = ClusterConfig::new(id_owned, members, dir, ADMIN.into());
                config.tick = Duration::from_millis(20);
                config.snapshot_every = snapshot_every;
                config.write_timeout = Duration::from_secs(3);
                Cluster::start(state.clone(), config).unwrap();
                ready_tx.send(()).unwrap();
                tokio::select! {
                    _ = axum::serve(listener, app(state)) => {}
                    _ = stopped => {}
                }
            });
        });
        ready_rx.recv().unwrap();
        Node { id: id.to_owned(), url: format!("http://{addr}"), stop: Some(stop), thread: Some(thread) }
    }

    fn kill(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

/// A cluster's addresses and directories, so nodes can be restarted.
struct Cluster3 {
    addrs: Vec<SocketAddr>,
    dirs: Vec<PathBuf>,
    _tmp: tempfile::TempDir,
    snapshot_every: u64,
}

impl Cluster3 {
    fn new(n: usize, snapshot_every: u64) -> Cluster3 {
        let tmp = tempfile::tempdir().unwrap();
        let addrs = (0..n).map(|_| free_port()).collect();
        let dirs = (0..n).map(|i| tmp.path().join(format!("node{i}"))).collect();
        Cluster3 { addrs, dirs, _tmp: tmp, snapshot_every }
    }

    fn members(&self) -> Vec<Member> {
        self.addrs.iter().enumerate().map(|(i, a)| Member { id: format!("n{i}"), url: format!("http://{a}") }).collect()
    }

    fn start(&self, i: usize) -> Node {
        Node::start(&format!("n{i}"), self.addrs[i], self.members(), &self.dirs[i], self.snapshot_every)
    }
}

/// Ports below the range the system hands out for port 0, so a stopped
/// node's port is not given to another test before the node restarts.
fn free_port() -> SocketAddr {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let base = 20_000 + (std::process::id() % 100) as u16 * 100;
    loop {
        let port = base + NEXT.fetch_add(1, Ordering::Relaxed) % 100;
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        if std::net::TcpListener::bind(addr).is_ok() {
            return addr;
        }
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(10)).build().unwrap()
}

async fn call(method: reqwest::Method, url: String, key: &str, body: Option<Value>) -> (u16, Value) {
    let mut request = client().request(method, url).header("X-Algolia-API-Key", key);
    if let Some(body) = body {
        request = request.body(body.to_string());
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
        }
        Err(e) => (0, Value::String(e.to_string())),
    }
}

async fn status(node: &Node) -> Value {
    call(reqwest::Method::GET, format!("{}/1/cluster/status", node.url), ADMIN, None).await.1
}

async fn put(node: &Node, id: &str, name: &str) -> (u16, Value) {
    let url = format!("{}/1/indexes/products/{id}", node.url);
    call(reqwest::Method::PUT, url, ADMIN, Some(json!({ "name": name }))).await
}

async fn ids(node: &Node, query: &str) -> Option<Vec<String>> {
    let url = format!("{}/1/indexes/products/query", node.url);
    let (status, r) = call(reqwest::Method::POST, url, SEARCH, Some(json!({"query": query, "hitsPerPage": 1000}))).await;
    if status != 200 {
        return None;
    }
    let mut ids: Vec<String> = r["hits"].as_array()?.iter().map(|h| h["objectID"].as_str().unwrap().to_owned()).collect();
    ids.sort();
    Some(ids)
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    while !check().await {
        assert!(start.elapsed() < Duration::from_secs(15), "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// The node the given nodes agree leads, once they do.
async fn leader_of(nodes: &[&Node]) -> String {
    leader_other_than(nodes, "").await
}

/// Like `leader_of`, waiting until the leader is not `dead` (which the
/// others still believe in until they notice it is gone).
async fn leader_other_than(nodes: &[&Node], dead: &str) -> String {
    let start = Instant::now();
    loop {
        let mut seen = Vec::new();
        for node in nodes {
            seen.push(status(node).await["leader"].as_str().map(str::to_owned));
        }
        if let Some(l) = &seen[0]
            && l != dead
            && seen.iter().all(|s| s.as_ref() == Some(l))
        {
            return l.clone();
        }
        assert!(start.elapsed() < Duration::from_secs(15), "no agreed leader: {seen:?}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// The JSON header line of a snapshot or image file.
fn header(path: &Path) -> Value {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    let end = bytes.iter().position(|&b| b == b'\n').unwrap();
    serde_json::from_slice(&bytes[..end]).unwrap()
}

fn expected(n: usize) -> Vec<String> {
    let mut ids: Vec<String> = (0..n).map(|i| format!("p{i}")).collect();
    ids.sort();
    ids
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_on_any_node_reach_every_node() {
    let c = Cluster3::new(3, 10_000);
    let nodes: Vec<Node> = (0..3).map(|i| c.start(i)).collect();
    let refs: Vec<&Node> = nodes.iter().collect();
    let leader = leader_of(&refs).await;
    // Writes through every node, the followers forwarding to the leader.
    for (i, node) in nodes.iter().enumerate() {
        let (status, r) = put(node, &format!("p{i}"), "Wireless kettle").await;
        assert_eq!(status, 200, "{r}");
        // Read your writes: the node that answered has it.
        assert!(ids(node, "kettle").await.unwrap().contains(&format!("p{i}")));
    }
    for node in &nodes {
        eventually("every node has every write", || async { ids(node, "kettle").await == Some(expected(3)) }).await;
    }
    // Settings, keys and index deletion go through the cluster too.
    let follower = nodes.iter().find(|n| n.id != leader).unwrap();
    let url = format!("{}/1/indexes/products/settings", follower.url);
    assert_eq!(call(reqwest::Method::PUT, url, ADMIN, Some(json!({"searchableAttributes": ["name"]}))).await.0, 200);
    let (code, created) = call(reqwest::Method::POST, format!("{}/1/keys", follower.url), ADMIN, Some(json!({"acl": ["search"]}))).await;
    assert_eq!(code, 200, "{created}");
    let key = created["key"].as_str().unwrap().to_owned();
    for node in &nodes {
        let url = format!("{}/1/indexes/products/query", node.url);
        eventually("the key works everywhere", || async {
            call(reqwest::Method::POST, url.clone(), &key, Some(json!({}))).await.0 == 200
        })
        .await;
    }
    let st = status(&nodes[0]).await;
    assert!(st["commitIndex"].as_u64().unwrap() >= 5, "{st}");
    for node in nodes {
        node.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_leader_takes_over_and_nothing_acknowledged_is_lost() {
    let c = Cluster3::new(3, 10_000);
    let mut nodes: Vec<Option<Node>> = (0..3).map(|i| Some(c.start(i))).collect();
    let refs: Vec<&Node> = nodes.iter().flatten().collect();
    let leader = leader_of(&refs).await;
    for i in 0..20 {
        let node = nodes[i % 3].as_ref().unwrap();
        assert_eq!(put(node, &format!("p{i}"), "kettle").await.0, 200);
    }
    // The leader dies.
    let li: usize = leader[1..].parse().unwrap();
    nodes[li].take().unwrap().kill();
    let started = Instant::now();
    let survivors: Vec<&Node> = nodes.iter().flatten().collect();
    let new_leader = leader_other_than(&survivors, &leader).await;
    let failover = started.elapsed();
    assert!(failover < Duration::from_secs(5), "failover took {failover:?}");
    // Every acknowledged write is still there, and writes go on.
    for node in &survivors {
        eventually("acknowledged writes survive", || async { ids(node, "kettle").await == Some(expected(20)) }).await;
    }
    for i in 20..30 {
        let node = survivors[i % 2];
        assert_eq!(put(node, &format!("p{i}"), "kettle").await.0, 200);
    }
    // The old leader returns and catches up from the log.
    nodes[li] = Some(c.start(li));
    let back = nodes[li].as_ref().unwrap();
    eventually("the returning node catches up", || async { ids(back, "kettle").await == Some(expected(30)) }).await;
    let refs: Vec<&Node> = nodes.iter().flatten().collect();
    assert_eq!(leader_of(&refs).await, new_leader, "the returning node took over");
    for node in nodes.into_iter().flatten() {
        node.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_far_behind_node_catches_up_from_a_snapshot() {
    let c = Cluster3::new(3, 15);
    let mut nodes: Vec<Option<Node>> = (0..3).map(|i| Some(c.start(i))).collect();
    let refs: Vec<&Node> = nodes.iter().flatten().collect();
    let leader = leader_of(&refs).await;
    let li: usize = leader[1..].parse().unwrap();
    let away = (li + 1) % 3;
    nodes[away].take().unwrap().kill();
    for i in 0..60 {
        assert_eq!(put(nodes[li].as_ref().unwrap(), &format!("p{i}"), "kettle").await.0, 200);
    }
    // The others have compacted their logs past what the absent node has.
    let st = status(nodes[li].as_ref().unwrap()).await;
    assert!(st["snapshotIndex"].as_u64().unwrap() >= 15, "{st}");
    nodes[away] = Some(c.start(away));
    let back = nodes[away].as_ref().unwrap();
    eventually("the node is restored from a snapshot", || async { ids(back, "kettle").await == Some(expected(60)) }).await;
    let st = status(back).await;
    assert!(st["snapshotIndex"].as_u64().unwrap() >= 15, "{st}");
    // The leader's image came with its snapshot.
    let dir = &c.dirs[away];
    assert_eq!(header(&dir.join("snapshot.image"))["lastIndex"], header(&dir.join("snapshot"))["lastIndex"]);
    // And keeps following.
    assert_eq!(put(back, "late", "kettle").await.0, 200);
    for node in nodes.iter().flatten() {
        eventually("everyone has the late write", || async {
            ids(node, "kettle").await.is_some_and(|ids| ids.contains(&"late".to_owned()))
        })
        .await;
    }
    for node in nodes.into_iter().flatten() {
        node.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_whole_cluster_restarts_with_its_data() {
    let c = Cluster3::new(3, 25);
    let nodes: Vec<Node> = (0..3).map(|i| c.start(i)).collect();
    let refs: Vec<&Node> = nodes.iter().collect();
    leader_of(&refs).await;
    for i in 0..40 {
        assert_eq!(put(&nodes[i % 3], &format!("p{i}"), "kettle").await.0, 200);
    }
    for node in nodes {
        node.kill();
    }
    // Every node has an image of its snapshot, for the same entry.
    for dir in &c.dirs {
        let snapshot = header(&dir.join("snapshot"));
        assert_eq!(header(&dir.join("snapshot.image"))["lastIndex"], snapshot["lastIndex"], "{dir:?}");
    }
    // One node's image is damaged: it loads from the records instead, and
    // writes a good image again.
    let image = c.dirs[0].join("snapshot.image");
    let mut bytes = std::fs::read(&image).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    std::fs::write(&image, &bytes).unwrap();
    let nodes: Vec<Node> = (0..3).map(|i| c.start(i)).collect();
    for node in &nodes {
        eventually("data back after a full restart", || async { ids(node, "kettle").await == Some(expected(40)) }).await;
    }
    assert_ne!(std::fs::read(&image).unwrap(), bytes, "the damaged image was replaced");
    assert_eq!(put(&nodes[0], "p40", "kettle").await.0, 200);
    for node in nodes {
        node.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_majority_writes_fail_but_searches_go_on() {
    let c = Cluster3::new(3, 10_000);
    let mut nodes: Vec<Option<Node>> = (0..3).map(|i| Some(c.start(i))).collect();
    let refs: Vec<&Node> = nodes.iter().flatten().collect();
    leader_of(&refs).await;
    assert_eq!(put(nodes[0].as_ref().unwrap(), "p0", "kettle").await.0, 200);
    for node in nodes.iter().flatten() {
        eventually("replicated", || async { ids(node, "kettle").await == Some(expected(1)) }).await;
    }
    nodes[1].take().unwrap().kill();
    nodes[2].take().unwrap().kill();
    let (code, body) = put(nodes[0].as_ref().unwrap(), "p1", "kettle").await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(ids(nodes[0].as_ref().unwrap(), "kettle").await, Some(expected(1)));
    // A majority again: writes work.
    nodes[1] = Some(c.start(1));
    let lone = nodes[0].as_ref().unwrap();
    let mut ok = false;
    for _ in 0..50 {
        if put(lone, "p1", "kettle").await.0 == 200 {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ok, "writes did not resume with a majority");
    for node in nodes.into_iter().flatten() {
        node.kill();
    }
}
