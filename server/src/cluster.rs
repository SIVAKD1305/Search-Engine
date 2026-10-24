//! A cluster of search servers that agree on every write with Raft, and
//! elect a new leader when the leader fails.
//!
//! Every node keeps all the data and serves searches. Writes go to the
//! leader (other nodes forward them); a write is answered once a majority
//! of nodes have it on disk and the node answering has applied it, so it
//! survives the failure of any minority, and a client sees its own writes.
//! With 3 nodes the cluster keeps working with one node down; with 5, with
//! two down.
//!
//! The Raft log is the write-ahead log here: every write is a `Command`,
//! applied in log order on every node. Every `snapshot_every` entries a node
//! saves its whole state (`AppState::export_state`) and drops the log before
//! it; a node too far behind receives the leader's snapshot.
//!
//! Nodes talk over HTTP with the admin key (all nodes share it):
//!
//! - `POST /1/cluster/messages`: Raft messages, batched.
//! - `POST /1/cluster/snapshot`: a snapshot for a node that is behind.
//! - `POST /1/cluster/propose`: a write forwarded to the leader.
//! - `GET /1/cluster/status`: this node's role, term, leader and progress.
//!
//! Searches read the node's own copy, which may lag the leader by a few
//! milliseconds, as with Algolia's replicas.

use crate::ApiError;
use crate::raft::{self, LogEntry, Message, NodeId, ProposeError, Raft, Role};
use crate::raft_store::RaftStore;
use crate::state::{AppState, Command};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

/// How long a write waits to be agreed on before giving up, by default.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct Member {
    pub id: NodeId,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct ClusterConfig {
    pub id: NodeId,
    pub members: Vec<Member>,
    /// Where the Raft log and snapshots live.
    pub dir: PathBuf,
    pub admin_key: String,
    pub tick: Duration,
    pub raft: raft::Config,
    /// Applied entries between snapshots.
    pub snapshot_every: u64,
    /// How long a write waits to be agreed on before giving up.
    pub write_timeout: Duration,
}

impl ClusterConfig {
    /// A node with the usual timing: 50 ms ticks, so elections after
    /// 0.5-1 s without a leader and heartbeats every 100 ms.
    pub fn new(id: NodeId, members: Vec<Member>, dir: PathBuf, admin_key: String) -> Self {
        ClusterConfig {
            id,
            members,
            dir,
            admin_key,
            tick: Duration::from_millis(50),
            raft: raft::Config::default(),
            snapshot_every: 10_000,
            write_timeout: WRITE_TIMEOUT,
        }
    }
}

impl ClusterConfig {
    /// `a=http://host:port,b=...`
    pub fn parse_members(spec: &str) -> Result<Vec<Member>, String> {
        let members: Vec<Member> = spec
            .split(',')
            .map(|m| {
                let (id, url) = m.split_once('=').ok_or(format!("expected id=url, got {m:?}"))?;
                Ok(Member { id: id.trim().to_owned(), url: url.trim().trim_end_matches('/').to_owned() })
            })
            .collect::<Result<_, String>>()?;
        if members.is_empty() {
            return Err("a cluster needs members".into());
        }
        Ok(members)
    }
}

/// What a node reports about itself.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub id: NodeId,
    pub role: Role,
    pub term: u64,
    pub leader: Option<NodeId>,
    pub commit_index: u64,
    pub applied_index: u64,
    pub last_index: u64,
    pub snapshot_index: u64,
    /// As the leader knows it: the last index each follower has.
    pub followers: HashMap<NodeId, u64>,
}

enum Event {
    Messages { from: NodeId, messages: Vec<Message> },
    /// A snapshot from `from`, saved at `path`.
    Snapshot { from: NodeId, term: u64, last_index: u64, last_term: u64, path: PathBuf },
    /// `applied` hears whether the entry at the proposal's index turned out
    /// to be it.
    Propose { command: String, reply: oneshot::Sender<Result<u64, ProposeError>>, applied: oneshot::Sender<bool> },
    SnapshotFailed(NodeId),
    /// The applier saved a snapshot up to this index.
    Snapshotted(u64),
}

enum Apply {
    Entries(Vec<LogEntry>),
    /// Load the snapshot just installed, which ends at this index.
    Restore(u64),
}

/// A proposal waiting to be applied: its term, and who to tell whether the
/// entry at its index was it.
type Waiter = (u64, oneshot::Sender<bool>);

pub struct Cluster {
    config: ClusterConfig,
    events: mpsc::UnboundedSender<Event>,
    status: Mutex<Status>,
    applied: watch::Sender<u64>,
    waiters: Mutex<HashMap<u64, Waiter>>,
    /// Held while the snapshot file is replaced, by the applier (its own
    /// snapshots) and the Raft loop (the leader's).
    snapshot_file: Mutex<()>,
    client: reqwest::Client,
}

fn unavailable(message: impl Into<String>) -> ApiError {
    ApiError { status: StatusCode::SERVICE_UNAVAILABLE, message: message.into() }
}

impl Cluster {
    /// Loads this node's Raft state and data, and starts the node's tasks.
    /// Must run inside a multi-threaded Tokio runtime.
    pub fn start(state: Arc<AppState>, config: ClusterConfig) -> io::Result<Arc<Cluster>> {
        let loaded = RaftStore::open(&config.dir)?;
        let mut snapshot = (0, 0);
        if let Some(header) = loaded.snapshot {
            let (_, bytes) = RaftStore::read_snapshot(&loaded.store.snapshot_path())?;
            state.import_state(&bytes).map_err(|e| io::Error::other(e.to_string()))?;
            snapshot = (header.last_index, header.last_term);
        }
        let ids: Vec<NodeId> = config.members.iter().map(|m| m.id.clone()).collect();
        if !ids.contains(&config.id) {
            return Err(io::Error::other(format!("node {} is not among the members {ids:?}", config.id)));
        }
        let raft = Raft::new(config.id.clone(), &ids, config.raft.clone(), loaded.hard, snapshot, loaded.entries, rand::random());
        let (events, events_rx) = mpsc::unbounded_channel();
        let client = reqwest::Client::builder().no_proxy().connect_timeout(Duration::from_secs(2)).build().expect("HTTP client");
        let status = Status {
            id: config.id.clone(),
            role: Role::Follower,
            term: raft.term(),
            leader: None,
            commit_index: snapshot.0,
            applied_index: snapshot.0,
            last_index: raft.last_index(),
            snapshot_index: snapshot.0,
            followers: HashMap::new(),
        };
        let cluster = Arc::new(Cluster {
            config,
            events,
            status: Mutex::new(status),
            applied: watch::channel(snapshot.0).0,
            waiters: Mutex::new(HashMap::new()),
            snapshot_file: Mutex::new(()),
            client,
        });
        let mut peers = HashMap::new();
        for member in cluster.config.members.iter().filter(|m| m.id != cluster.config.id) {
            let (tx, rx) = mpsc::unbounded_channel();
            peers.insert(member.id.clone(), tx);
            tokio::spawn(send_to_peer(cluster.clone(), member.clone(), rx));
        }
        let (apply_tx, apply_rx) = std::sync::mpsc::channel();
        let snapshot_path = loaded.store.snapshot_path();
        {
            let (cluster, state) = (cluster.clone(), state.clone());
            std::thread::spawn(move || apply_loop(cluster, state, apply_rx, snapshot.0, snapshot_path));
        }
        tokio::spawn(raft_loop(cluster.clone(), raft, loaded.store, events_rx, apply_tx, peers));
        state.cluster.set(cluster.clone()).map_err(|_| io::Error::other("cluster already started"))?;
        Ok(cluster)
    }

    pub fn status(&self) -> Status {
        let mut status = self.status.lock().unwrap().clone();
        status.applied_index = *self.applied.borrow();
        status
    }

    fn url_of(&self, id: &NodeId) -> Option<&str> {
        self.config.members.iter().find(|m| m.id == *id).map(|m| m.url.as_str())
    }

    /// Agrees on `command` with the cluster and applies it here. Returns
    /// once this node has applied it.
    pub async fn submit(&self, command: &Command) -> Result<(), ApiError> {
        let text = serde_json::to_string(command).expect("commands serialize");
        let index = tokio::time::timeout(self.config.write_timeout, self.agree(text))
            .await
            .map_err(|_| unavailable("the cluster did not confirm the write in time; it may still be applied"))??;
        self.wait_applied(index).await
    }

    /// Gets `command` committed and applied on the leader, from here or by
    /// forwarding it. Returns its log index.
    async fn agree(&self, text: String) -> Result<u64, ApiError> {
        loop {
            match self.propose_here(text.clone()).await {
                Ok(index) => return Ok(index),
                Err(ProposeError::NotLeader(Some(leader))) => {
                    let url = self.url_of(&leader).ok_or_else(|| unavailable("unknown leader"))?;
                    match self.forward(url, &text).await {
                        Forwarded::Done(index) => return Ok(index),
                        Forwarded::Failed(e) => return Err(e),
                        // The leader changed before it took the write: try again.
                        Forwarded::NotLeader => {}
                    }
                }
                Err(ProposeError::NotLeader(None)) => {}
            }
            // No leader yet (an election is under way).
            tokio::time::sleep(self.config.tick * 2).await;
        }
    }

    /// Proposes on this node if it leads, and waits until the entry applies.
    async fn propose_here(&self, text: String) -> Result<u64, ProposeError> {
        let (reply, response) = oneshot::channel();
        let (tell, applied) = oneshot::channel();
        let _ = self.events.send(Event::Propose { command: text, reply, applied: tell });
        let index = response.await.map_err(|_| ProposeError::NotLeader(None))??;
        // The entry at `index` may yet be another leader's.
        match applied.await {
            Ok(true) => Ok(index),
            _ => Err(ProposeError::NotLeader(None)),
        }
    }

    async fn forward(&self, url: &str, text: &str) -> Forwarded {
        let request = self
            .client
            .post(format!("{url}/1/cluster/propose"))
            .header("X-Algolia-API-Key", &self.config.admin_key)
            .timeout(self.config.write_timeout)
            .body(json!({ "command": text }).to_string());
        let response = match request.send().await {
            Ok(r) => r,
            // Not delivered: the leader is unreachable, maybe gone.
            Err(e) if e.is_connect() => return Forwarded::NotLeader,
            Err(e) => return Forwarded::Failed(unavailable(format!("forwarding the write failed: {e}"))),
        };
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        match (status, body["index"].as_u64()) {
            (StatusCode::OK, Some(index)) => Forwarded::Done(index),
            (StatusCode::MISDIRECTED_REQUEST, _) => Forwarded::NotLeader,
            _ => Forwarded::Failed(unavailable(format!("the leader answered {status}: {body}"))),
        }
    }

    async fn wait_applied(&self, index: u64) -> Result<(), ApiError> {
        let mut applied = self.applied.subscribe();
        tokio::time::timeout(self.config.write_timeout, applied.wait_for(|a| *a >= index))
            .await
            .map_err(|_| unavailable("the write is agreed on but not yet applied here"))?
            .map_err(|_| unavailable("the node is stopping"))?;
        Ok(())
    }

    /// Called by the applier for each entry it applied.
    fn applied(&self, index: u64, term: u64) {
        if let Some((expected, tell)) = self.waiters.lock().unwrap().remove(&index) {
            let _ = tell.send(expected == term);
        }
    }
}

enum Forwarded {
    Done(u64),
    NotLeader,
    Failed(ApiError),
}

/// Owns the Raft state machine and its storage: feeds it events and ticks,
/// saves what it says to save, then sends and applies.
async fn raft_loop(
    cluster: Arc<Cluster>,
    mut raft: Raft,
    mut store: RaftStore,
    mut events: mpsc::UnboundedReceiver<Event>,
    apply: std::sync::mpsc::Sender<Apply>,
    peers: HashMap<NodeId, mpsc::UnboundedSender<Message>>,
) {
    let mut ticks = tokio::time::interval(cluster.config.tick);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut incoming: Option<PathBuf> = None;
    loop {
        let mut rewrite_log = false;
        let mut handle = |event: Event, raft: &mut Raft| match event {
            Event::Messages { from, messages } => {
                for message in messages {
                    raft.step(&from, message);
                }
            }
            Event::Snapshot { from, term, last_index, last_term, path } => {
                if let Some(old) = incoming.replace(path) {
                    let _ = std::fs::remove_file(old);
                }
                raft.step(&from, Message::InstallSnapshot { term, last_index, last_term });
            }
            Event::Propose { command, reply, applied } => {
                let result = raft.propose(command).map(|(index, term)| {
                    // Registered before the entry can commit.
                    cluster.waiters.lock().unwrap().insert(index, (term, applied));
                    index
                });
                let _ = reply.send(result);
            }
            Event::SnapshotFailed(peer) => raft.snapshot_failed(&peer),
            Event::Snapshotted(index) => {
                raft.compact(index);
                rewrite_log = true;
            }
        };
        tokio::select! {
            _ = ticks.tick() => raft.tick(),
            event = events.recv() => {
                let Some(event) = event else { return };
                handle(event, &mut raft);
                // Take what else is waiting, so it is saved with one fsync.
                for _ in 0..1000 {
                    match events.try_recv() {
                        Ok(event) => handle(event, &mut raft),
                        Err(_) => break,
                    }
                }
            }
        }
        let ready = raft.take_ready();

        // 1. Save. A snapshot received from the leader goes into place
        //    before the log it replaces is cut.
        let saved = tokio::task::block_in_place(|| -> io::Result<()> {
            if ready.restore.is_some() {
                let path = incoming.take().expect("a restore follows a received snapshot");
                let _file = cluster.snapshot_file.lock().unwrap();
                std::fs::rename(&path, store.snapshot_path())?;
            }
            if let Some(hard) = &ready.hard_state {
                store.save_hard_state(hard)?;
            }
            if let Some(from) = ready.truncate_from {
                store.write_log(from, &ready.entries)?;
            }
            if rewrite_log {
                store.rewrite(raft.entries())?;
            }
            Ok(())
        });
        if let Err(e) = saved {
            // Without its promises on disk the node must not go on.
            eprintln!("cluster: cannot save Raft state: {e}; stopping");
            std::process::exit(1);
        }

        // 2. Load a received snapshot (before any entry after it applies).
        if let Some((index, _)) = ready.restore {
            let _ = apply.send(Apply::Restore(index));
        }

        // 3. Send.
        for (to, message) in ready.messages {
            if let Message::InstallSnapshot { term, .. } = message {
                tokio::spawn(send_snapshot(cluster.clone(), to, term, store.snapshot_path()));
            } else if let Some(peer) = peers.get(&to) {
                let _ = peer.send(message);
            }
        }

        // 4. Apply.
        if !ready.committed.is_empty() {
            let _ = apply.send(Apply::Entries(ready.committed));
        }

        let mut status = cluster.status.lock().unwrap();
        status.role = raft.role();
        status.term = raft.term();
        status.leader = raft.leader().cloned();
        status.commit_index = raft.commit_index();
        status.last_index = raft.last_index();
        status.snapshot_index = raft.snapshot_position().0;
        status.followers = if raft.role() == Role::Leader { raft.matched().into_iter().collect() } else { HashMap::new() };
    }
}

/// Applies committed entries in order, and snapshots now and then.
fn apply_loop(
    cluster: Arc<Cluster>,
    state: Arc<AppState>,
    entries: std::sync::mpsc::Receiver<Apply>,
    mut applied: u64,
    snapshot_path: PathBuf,
) {
    let mut last_snapshot = applied;
    let mut applied_term = 0;
    for work in entries {
        match work {
            Apply::Restore(expected) => match RaftStore::read_snapshot(&snapshot_path) {
                Ok((header, bytes)) => {
                    assert_eq!(header.last_index, expected, "the snapshot file is the one received");
                    if let Err(e) = state.import_state(&bytes) {
                        eprintln!("cluster: cannot load the leader's snapshot: {e}; stopping");
                        std::process::exit(1);
                    }
                    applied = header.last_index;
                    applied_term = header.last_term;
                    last_snapshot = applied;
                    println!("cluster: loaded a snapshot up to entry {applied}");
                }
                Err(e) => {
                    eprintln!("cluster: cannot read the received snapshot: {e}; stopping");
                    std::process::exit(1);
                }
            },
            Apply::Entries(list) => {
                for entry in list {
                    if entry.index <= applied {
                        continue;
                    }
                    if !entry.command.is_empty() {
                        match serde_json::from_str::<Command>(&entry.command) {
                            Ok(command) => {
                                if let Err(e) = state.apply_command(command, entry.index) {
                                    eprintln!("cluster: applying entry {}: {e}", entry.index);
                                }
                            }
                            Err(e) => eprintln!("cluster: entry {} is not a command: {e}", entry.index),
                        }
                    }
                    applied = entry.index;
                    applied_term = entry.term;
                    cluster.applied(entry.index, entry.term);
                }
            }
        }
        cluster.applied.send_replace(applied);
        if applied >= last_snapshot + cluster.config.snapshot_every {
            let bytes = state.export_state();
            let _file = cluster.snapshot_file.lock().unwrap();
            // A newer snapshot from the leader may be in place, waiting to
            // be loaded: never replace it with an older one.
            let current = RaftStore::snapshot_index(&snapshot_path).unwrap_or(None).unwrap_or(0);
            if current >= applied {
                continue;
            }
            match RaftStore::write_snapshot(&snapshot_path, applied, applied_term, &bytes) {
                Ok(_) => {
                    last_snapshot = applied;
                    let _ = cluster.events.send(Event::Snapshotted(applied));
                }
                Err(e) => eprintln!("cluster: snapshot failed: {e}"),
            }
        }
    }
}

/// Sends a peer its messages in order, a batch per request. Undeliverable
/// messages are dropped: Raft sends again.
async fn send_to_peer(cluster: Arc<Cluster>, peer: Member, mut queue: mpsc::UnboundedReceiver<Message>) {
    let url = format!("{}/1/cluster/messages", peer.url);
    while let Some(first) = queue.recv().await {
        let mut batch = vec![first];
        while batch.len() < 256 {
            match queue.try_recv() {
                Ok(m) => batch.push(m),
                Err(_) => break,
            }
        }
        let body = json!({ "from": cluster.config.id, "messages": batch }).to_string();
        let _ = cluster
            .client
            .post(&url)
            .header("X-Algolia-API-Key", &cluster.config.admin_key)
            .timeout(Duration::from_secs(5))
            .body(body)
            .send()
            .await;
    }
}

/// Sends this node's current snapshot to a follower that is too far behind.
async fn send_snapshot(cluster: Arc<Cluster>, peer: NodeId, term: u64, path: PathBuf) {
    let result: Result<(), String> = async {
        let url = cluster.url_of(&peer).ok_or("unknown peer")?.to_owned();
        let bytes = tokio::fs::read(&path).await.map_err(|e| e.to_string())?;
        // The file may be newer than the message that asked for it: send
        // what it holds.
        let (header, _) = RaftStore::parse_snapshot(&bytes)?;
        let query =
            format!("from={}&term={term}&lastIndex={}&lastTerm={}", cluster.config.id, header.last_index, header.last_term);
        let response = cluster
            .client
            .post(format!("{url}/1/cluster/snapshot?{query}"))
            .header("X-Algolia-API-Key", &cluster.config.admin_key)
            .timeout(Duration::from_secs(600))
            .body(bytes)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        response.error_for_status().map(|_| ()).map_err(|e| e.to_string())
    }
    .await;
    if let Err(e) = result {
        eprintln!("cluster: sending a snapshot to {peer} failed: {e}");
        let _ = cluster.events.send(Event::SnapshotFailed(peer));
    }
}

// ---- HTTP endpoints (admin key; mounted by `app`) ----

pub fn routes() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/1/cluster/messages", post(receive_messages))
        .route("/1/cluster/snapshot", post(receive_snapshot))
        .route("/1/cluster/propose", post(receive_proposal))
        .route("/1/cluster/status", get(status))
        // Batches of large writes and whole snapshots.
        .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024 * 1024))
}

fn cluster_of(state: &AppState) -> Result<&Arc<Cluster>, ApiError> {
    state.cluster.get().ok_or_else(|| ApiError::not_found("this server is not part of a cluster"))
}

#[derive(Deserialize)]
struct Messages {
    from: NodeId,
    messages: Vec<Message>,
}

async fn receive_messages(State(state): State<Arc<AppState>>, body: Bytes) -> Result<Json<Value>, ApiError> {
    let cluster = cluster_of(&state)?;
    let Messages { from, messages } =
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("bad messages: {e}")))?;
    let _ = cluster.events.send(Event::Messages { from, messages });
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotQuery {
    from: NodeId,
    term: u64,
    last_index: u64,
    last_term: u64,
}

async fn receive_snapshot(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SnapshotQuery>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let cluster = cluster_of(&state)?;
    let (header, _) = RaftStore::parse_snapshot(&body).map_err(ApiError::bad_request)?;
    if (header.last_index, header.last_term) != (q.last_index, q.last_term) {
        return Err(ApiError::bad_request("snapshot header does not match the request"));
    }
    let path = cluster.config.dir.join(format!("snapshot.incoming-{:016x}", rand::random::<u64>()));
    let written = tokio::task::spawn_blocking({
        let path = path.clone();
        move || -> io::Result<()> {
            use std::io::Write;
            let mut file = std::fs::File::create(&path)?;
            file.write_all(&body)?;
            file.sync_all()
        }
    })
    .await
    .expect("write task");
    written.map_err(|e| ApiError::storage(e.into()))?;
    let event = Event::Snapshot { from: q.from, term: q.term, last_index: q.last_index, last_term: q.last_term, path };
    let _ = cluster.events.send(event);
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct Proposal {
    command: String,
}

/// A write forwarded by another node: taken only by the leader.
async fn receive_proposal(State(state): State<Arc<AppState>>, body: Bytes) -> Result<Json<Value>, ApiError> {
    let cluster = cluster_of(&state)?;
    let Proposal { command } =
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("bad proposal: {e}")))?;
    serde_json::from_str::<Command>(&command).map_err(|e| ApiError::bad_request(format!("bad command: {e}")))?;
    match tokio::time::timeout(cluster.config.write_timeout, cluster.propose_here(command)).await {
        Ok(Ok(index)) => Ok(Json(json!({ "index": index }))),
        Ok(Err(ProposeError::NotLeader(leader))) => Err(ApiError {
            status: StatusCode::MISDIRECTED_REQUEST,
            message: format!("not the leader; the leader is {}", leader.as_deref().unwrap_or("unknown")),
        }),
        Err(_) => Err(unavailable("the cluster did not confirm the write in time")),
    }
}

async fn status(State(state): State<Arc<AppState>>) -> Result<Json<Status>, ApiError> {
    Ok(Json(cluster_of(&state)?.status()))
}
