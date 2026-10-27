//! Raft clusters of real servers on real sockets, in one process. Each node
//! has its own runtime on its own thread, so killing one closes every socket
//! at once, as when a process dies; its data directory stays for a restart.
//! Nodes beyond the first few (`Cluster3::with_pool`) start in join mode and
//! wait to be added.

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
    fn start(
        id: &str,
        addr: SocketAddr,
        members: Vec<Member>,
        dir: &Path,
        snapshot_every: u64,
        member_timeout: Duration,
    ) -> Node {
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
                config.member_timeout = member_timeout;
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

/// A cluster's addresses and directories, so nodes can be restarted. The
/// first `bootstrap` nodes start the cluster; the others join it later.
struct Cluster3 {
    addrs: Vec<SocketAddr>,
    dirs: Vec<PathBuf>,
    bootstrap: usize,
    _tmp: tempfile::TempDir,
    snapshot_every: u64,
    member_timeout: Duration,
}

impl Cluster3 {
    fn new(n: usize, snapshot_every: u64) -> Cluster3 {
        Cluster3::with_pool(n, n, snapshot_every)
    }

    fn with_pool(pool: usize, bootstrap: usize, snapshot_every: u64) -> Cluster3 {
        let tmp = tempfile::tempdir().unwrap();
        let addrs = (0..pool).map(|_| free_port()).collect();
        let dirs = (0..pool).map(|i| tmp.path().join(format!("node{i}"))).collect();
        Cluster3 { addrs, dirs, bootstrap, _tmp: tmp, snapshot_every, member_timeout: Duration::from_secs(30) }
    }

    /// The cluster the first nodes start in.
    fn members(&self) -> Vec<Member> {
        (0..self.bootstrap).map(|i| Member { id: format!("n{i}"), url: format!("http://{}", self.addrs[i]) }).collect()
    }

    /// Starts node `i`: a founder with the member list, or a joiner without one.
    fn start(&self, i: usize) -> Node {
        let members = if i < self.bootstrap { self.members() } else { Vec::new() };
        Node::start(&format!("n{i}"), self.addrs[i], members, &self.dirs[i], self.snapshot_every, self.member_timeout)
    }

    fn url(&self, i: usize) -> String {
        format!("http://{}", self.addrs[i])
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

// ---- membership changes ----

async fn add_node(via: &Node, c: &Cluster3, i: usize) -> (u16, Value) {
    let body = json!({ "id": format!("n{i}"), "url": c.url(i) });
    call(reqwest::Method::POST, format!("{}/1/cluster/members", via.url), ADMIN, Some(body)).await
}

async fn remove_node(via: &Node, id: &str) -> (u16, Value) {
    call(reqwest::Method::DELETE, format!("{}/1/cluster/members/{id}", via.url), ADMIN, None).await
}

/// (voters, learners) as one node sees them, sorted.
async fn members_seen_by(node: &Node) -> (Vec<String>, Vec<String>) {
    let (_, body) = call(reqwest::Method::GET, format!("{}/1/cluster/members", node.url), ADMIN, None).await;
    let ids = |key: &str| {
        let mut ids: Vec<String> =
            body[key].as_array().map(|a| a.iter().map(|m| m["id"].as_str().unwrap().to_owned()).collect()).unwrap_or_default();
        ids.sort();
        ids
    };
    (ids("voters"), ids("learners"))
}

fn names(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

/// A slot for every node of the pool, with the founders started.
fn founders(c: &Cluster3) -> Vec<Option<Node>> {
    (0..c.addrs.len()).map(|i| (i < c.bootstrap).then(|| c.start(i))).collect()
}

/// The live nodes among `nodes`.
fn live(nodes: &[Option<Node>]) -> Vec<&Node> {
    nodes.iter().flatten().collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nodes_are_added_and_removed_while_the_cluster_runs() {
    let c = Cluster3::with_pool(5, 3, 15);
    let mut nodes = founders(&c);
    let leader = leader_of(&live(&nodes)).await;
    for i in 0..40 {
        assert_eq!(put(nodes[i % 3].as_ref().unwrap(), &format!("p{i}"), "kettle").await.0, 200);
    }

    // A newcomer waits, knowing nothing, until the cluster adds it.
    nodes[3] = Some(c.start(3));
    let st = status(nodes[3].as_ref().unwrap()).await;
    assert_eq!((st["voter"].clone(), st["leader"].clone()), (json!(false), Value::Null), "{st}");
    // Any node takes the request; it returns once the newcomer is a voter.
    let follower = nodes[..3].iter().flatten().find(|n| n.id != leader).unwrap();
    let (code, body) = add_node(follower, &c, 3).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["members"]["voters"].as_array().unwrap().len(), 4, "{body}");
    let n3 = nodes[3].as_ref().unwrap();
    eventually("the newcomer has all the data", || async { ids(n3, "kettle").await == Some(expected(40)) }).await;
    for node in live(&nodes) {
        // Followers learn of it a heartbeat after the leader.
        eventually("everyone sees four voters", || async {
            members_seen_by(node).await.0 == names(&["n0", "n1", "n2", "n3"])
        })
        .await;
    }
    // It takes part: a write through it reaches everybody.
    assert_eq!(put(n3, "p40", "kettle").await.0, 200);
    for node in live(&nodes) {
        eventually("the newcomer's write is everywhere", || async { ids(node, "kettle").await == Some(expected(41)) }).await;
    }

    // A fifth, then two nodes down: three of five still agree on writes.
    nodes[4] = Some(c.start(4));
    let (code, body) = add_node(nodes[0].as_ref().unwrap(), &c, 4).await;
    assert_eq!(code, 200, "{body}");
    nodes[3].take().unwrap().kill();
    nodes[4].take().unwrap().kill();
    let writer = nodes[1].as_ref().unwrap();
    assert_eq!(put(writer, "p41", "kettle").await.0, 200);
    nodes[3] = Some(c.start(3));
    nodes[4] = Some(c.start(4));
    for node in live(&nodes) {
        eventually("restarted members catch up", || async { ids(node, "kettle").await == Some(expected(42)) }).await;
        eventually("everyone sees five voters", || async { members_seen_by(node).await.0.len() == 5 }).await;
    }

    // Shrink: remove n0, then n1, one at a time, through different nodes.
    let (code, body) = remove_node(nodes[2].as_ref().unwrap(), "n0").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["members"]["voters"].as_array().unwrap().len(), 4, "{body}");
    // The removed node is told, and no longer a member.
    let removed = nodes[0].as_ref().unwrap();
    eventually("the removed node learns it was removed", || async { status(removed).await["member"] == json!(false) }).await;
    let (code, body) = remove_node(nodes[4].as_ref().unwrap(), "n1").await;
    assert_eq!(code, 200, "{body}");
    nodes[0].take().unwrap().kill();
    nodes[1].take().unwrap().kill();
    let rest = live(&nodes);
    leader_of(&rest).await;
    for node in &rest {
        eventually("everyone sees three voters", || async {
            members_seen_by(node).await.0 == names(&["n2", "n3", "n4"])
        })
        .await;
    }
    assert_eq!(put(rest[0], "p42", "kettle").await.0, 200);
    for node in &rest {
        eventually("the smaller cluster has everything", || async { ids(node, "kettle").await == Some(expected(43)) }).await;
    }
    for node in nodes.into_iter().flatten() {
        node.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_node_is_replaced() {
    let c = Cluster3::with_pool(4, 3, 20);
    let mut nodes = founders(&c);
    leader_of(&live(&nodes)).await;
    for i in 0..25 {
        assert_eq!(put(nodes[i % 3].as_ref().unwrap(), &format!("p{i}"), "kettle").await.0, 200);
    }
    nodes[2].take().unwrap().kill();
    // Replace it: add a new node, then remove the dead one.
    nodes[3] = Some(c.start(3));
    let (code, body) = add_node(nodes[0].as_ref().unwrap(), &c, 3).await;
    assert_eq!(code, 200, "{body}");
    let (code, body) = remove_node(nodes[1].as_ref().unwrap(), "n2").await;
    assert_eq!(code, 200, "{body}");
    let alive = live(&nodes);
    for node in &alive {
        eventually("everyone sees the replacement", || async {
            members_seen_by(node).await.0 == names(&["n0", "n1", "n3"])
        })
        .await;
    }
    // Every acknowledged write is on the newcomer, and one of the old nodes
    // can now fail without stopping the cluster.
    eventually("the replacement has the data", || async { ids(alive[2], "kettle").await == Some(expected(25)) }).await;
    nodes[0].take().unwrap().kill();
    let alive = live(&nodes);
    leader_of(&alive).await;
    assert_eq!(put(alive[0], "p25", "kettle").await.0, 200);
    for node in &alive {
        eventually("two of three agree", || async { ids(node, "kettle").await == Some(expected(26)) }).await;
    }
    for node in nodes.into_iter().flatten() {
        node.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_leader_can_be_removed_and_the_others_take_over() {
    let c = Cluster3::new(3, 10_000);
    let nodes: Vec<Node> = (0..3).map(|i| c.start(i)).collect();
    let refs: Vec<&Node> = nodes.iter().collect();
    let leader = leader_of(&refs).await;
    for i in 0..10 {
        assert_eq!(put(&nodes[i % 3], &format!("p{i}"), "kettle").await.0, 200);
    }
    let old = nodes.iter().find(|n| n.id == leader).unwrap();
    let (code, body) = remove_node(old, &leader).await;
    assert_eq!(code, 200, "{body}");
    let others: Vec<&Node> = nodes.iter().filter(|n| n.id != leader).collect();
    let new = leader_other_than(&others, &leader).await;
    assert_ne!(new, leader);
    // The old leader knows, and keeps out of the way.
    eventually("the old leader is out", || async {
        let st = status(old).await;
        st["member"] == json!(false) && st["role"] != json!("leader")
    })
    .await;
    let term = status(others[0]).await["term"].as_u64().unwrap();
    assert_eq!(put(others[0], "p10", "kettle").await.0, 200);
    for node in &others {
        eventually("the others have everything", || async { ids(node, "kettle").await == Some(expected(11)) }).await;
        eventually("the others see two voters", || async { members_seen_by(node).await.0.len() == 2 }).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(status(others[0]).await["term"].as_u64().unwrap(), term, "the removed node kept disturbing the others");
    for node in nodes {
        node.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn membership_survives_a_full_restart() {
    let c = Cluster3::with_pool(4, 3, 12);
    let nodes: Vec<Node> = (0..3).map(|i| c.start(i)).collect();
    leader_of(&nodes.iter().collect::<Vec<_>>()).await;
    for i in 0..30 {
        assert_eq!(put(&nodes[i % 3], &format!("p{i}"), "kettle").await.0, 200);
    }
    let n3 = c.start(3);
    let (code, body) = add_node(&nodes[1], &c, 3).await;
    assert_eq!(code, 200, "{body}");
    for i in 30..35 {
        assert_eq!(put(&n3, &format!("p{i}"), "kettle").await.0, 200);
    }
    for node in nodes {
        node.kill();
    }
    n3.kill();

    // The founders are started with their old three-node list, and the
    // newcomer with --join: the membership comes from what they saved.
    let nodes: Vec<Node> = (0..4).map(|i| c.start(i)).collect();
    leader_of(&nodes.iter().collect::<Vec<_>>()).await;
    for node in &nodes {
        eventually("data and members are back", || async { ids(node, "kettle").await == Some(expected(35)) }).await;
        assert_eq!(members_seen_by(node).await.0, names(&["n0", "n1", "n2", "n3"]), "{}", node.id);
    }
    assert_eq!(put(&nodes[3], "p35", "kettle").await.0, 200);
    for node in nodes {
        node.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cluster_shrinks_to_one_node_and_grows_again() {
    let c = Cluster3::with_pool(4, 3, 10_000);
    let mut nodes = founders(&c);
    let leader = leader_of(&live(&nodes)).await;
    for i in 0..5 {
        assert_eq!(put(nodes[i % 3].as_ref().unwrap(), &format!("p{i}"), "kettle").await.0, 200);
    }
    // Keep the leader, remove the other two.
    let li: usize = leader[1..].parse().unwrap();
    for i in (0..3).filter(|i| *i != li) {
        let (code, body) = remove_node(nodes[li].as_ref().unwrap(), &format!("n{i}")).await;
        assert_eq!(code, 200, "{body}");
        nodes[i].take().unwrap().kill();
    }
    assert_eq!(members_seen_by(nodes[li].as_ref().unwrap()).await.0, vec![leader.clone()]);
    assert_eq!(put(nodes[li].as_ref().unwrap(), "p5", "kettle").await.0, 200);
    // And back up with a node that never was part of it.
    nodes[3] = Some(c.start(3));
    let (code, body) = add_node(nodes[li].as_ref().unwrap(), &c, 3).await;
    assert_eq!(code, 200, "{body}");
    let n3 = nodes[3].as_ref().unwrap();
    eventually("the new node has everything", || async { ids(n3, "kettle").await == Some(expected(6)) }).await;
    for node in nodes.into_iter().flatten() {
        node.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_membership_requests_are_refused_and_an_unreachable_node_can_be_retried() {
    let mut c = Cluster3::with_pool(4, 3, 10_000);
    c.member_timeout = Duration::from_secs(2);
    let nodes: Vec<Node> = (0..3).map(|i| c.start(i)).collect();
    leader_of(&nodes.iter().collect::<Vec<_>>()).await;
    let via = &nodes[0];

    // Already a voter; not a member; not a url; wrong key.
    assert_eq!(add_node(via, &c, 1).await.0, 409);
    assert_eq!(remove_node(via, "nobody").await.0, 400);
    let body = json!({ "id": "n3", "url": "not a url" });
    assert_eq!(call(reqwest::Method::POST, format!("{}/1/cluster/members", via.url), ADMIN, Some(body)).await.0, 400);
    let (code, _) = call(reqwest::Method::POST, format!("{}/1/cluster/members", via.url), SEARCH, Some(json!({}))).await;
    assert_eq!(code, 403);

    // The node is not running: the add times out but leaves it as a learner,
    // which does not hold the cluster back.
    let (code, body) = add_node(via, &c, 3).await;
    assert_eq!(code, 504, "{body}");
    assert!(body["message"].as_str().unwrap().contains("has not caught up"), "{body}");
    assert_eq!(members_seen_by(via).await, (names(&["n0", "n1", "n2"]), names(&["n3"])));
    assert_eq!(put(via, "p0", "kettle").await.0, 200);
    // A learner can be removed again, and added again.
    let (code, body) = remove_node(&nodes[1], "n3").await;
    assert_eq!(code, 200, "{body}");
    eventually("the learner is gone", || async { members_seen_by(via).await.1.is_empty() }).await;
    // Once the node runs, the request finishes the job.
    let n3 = c.start(3);
    let (code, body) = add_node(&nodes[2], &c, 3).await;
    assert_eq!(code, 200, "{body}");
    for node in nodes.iter().chain([&n3]) {
        eventually("everyone sees four voters", || async {
            members_seen_by(node).await.0 == names(&["n0", "n1", "n2", "n3"])
        })
        .await;
    }
    eventually("it has the data", || async { ids(&n3, "kettle").await == Some(expected(1)) }).await;
    for node in nodes {
        node.kill();
    }
    n3.kill();
}
