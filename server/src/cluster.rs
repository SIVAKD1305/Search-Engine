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
//! - `GET /1/cluster/members`: who is in the cluster.
//! - `POST /1/cluster/members` `{"id", "url"}`: adds a node. It joins as a
//!   learner, receives the log or a snapshot, and becomes a voter once it has
//!   caught up; the request returns then. Start the new node with `--join`
//!   (and no `--cluster`) first; any node takes the request and passes it to
//!   the leader.
//! - `DELETE /1/cluster/members/{id}`: removes a node (a voter or a learner,
//!   one at a time). A removed node is not told: stop its process.
//!
//! Voters change one at a time (see `raft`), so a cluster can grow from one
//! node to five and shrink again, replacing nodes that died, without ever
//! stopping.
//!
//! Searches read the node's own copy, which may lag the leader by a few
//! milliseconds, as with Algolia's replicas.

use crate::ApiError;
use crate::raft::{self, LogEntry, Membership, MembershipError, Message, NodeId, ProposeError, Raft, Role};
pub use crate::raft::Member;
use crate::raft_store::RaftStore;
use crate::state::{AppState, Command, StateView};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};

/// How long a write waits to be agreed on before giving up, by default.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct ClusterConfig {
    pub id: NodeId,
    /// The cluster to start in, on the first start (later starts use the
    /// membership in the log and snapshots). Empty: wait to be added.
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
    /// How long adding a node may take (it waits for the node to catch up).
    pub member_timeout: Duration,
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
            member_timeout: Duration::from_secs(600),
        }
    }
}

/// A learner is promoted once it is within this many entries of the log.
const CATCH_UP_ENTRIES: u64 = 100;

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
        let mut ids: Vec<&NodeId> = members.iter().map(|m| &m.id).collect();
        ids.sort();
        ids.dedup();
        if ids.len() != members.len() || members.iter().any(|m| m.id.is_empty() || m.url.is_empty()) {
            return Err("every member needs a distinct id and a url".into());
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
    /// As the leader knows it: the last index each follower (and learner) has.
    pub followers: HashMap<NodeId, u64>,
    /// The membership in effect on this node.
    pub members: Membership,
    /// Whether this node is one of the members (a removed node is not).
    pub member: bool,
    /// Whether this node is a voter.
    pub voter: bool,
}

/// One change to the membership.
#[derive(Debug, Clone)]
enum MembershipChange {
    AddLearner(Member),
    Promote(NodeId),
    Remove(NodeId),
}

impl MembershipChange {
    /// The membership after the change.
    fn apply(&self, current: &Membership) -> Result<Membership, String> {
        let mut new = current.clone();
        match self {
            MembershipChange::AddLearner(member) => {
                if member.id.is_empty() || member.url.is_empty() {
                    return Err("a node needs an id and a url".into());
                }
                if current.contains(&member.id) {
                    return Err(format!("{} is already in the cluster", member.id));
                }
                new.learners.push(member.clone());
            }
            MembershipChange::Promote(id) => {
                let Some(position) = new.learners.iter().position(|m| m.id == *id) else {
                    return Err(format!("{id} is not a learner"));
                };
                let member = new.learners.remove(position);
                new.voters.push(member);
            }
            MembershipChange::Remove(id) => {
                if !current.contains(id) {
                    return Err(format!("{id} is not in the cluster"));
                }
                new.voters.retain(|m| m.id != *id);
                new.learners.retain(|m| m.id != *id);
            }
        }
        Ok(new)
    }
}

/// Why a membership change was not made.
#[derive(Debug)]
enum ChangeError {
    NotLeader,
    /// Not possible yet (a previous change is still committing): try again.
    Later,
    Invalid(String),
}

enum Event {
    Messages { from: NodeId, messages: Vec<Message> },
    /// A snapshot from `from`, saved at `path`, with the membership its
    /// header holds (none in snapshots from before membership changes).
    Snapshot { from: NodeId, term: u64, last_index: u64, last_term: u64, members: Option<Membership>, path: PathBuf },
    /// `applied` hears whether the entry at the change's index turned out
    /// to be it.
    ChangeMembership {
        change: MembershipChange,
        reply: oneshot::Sender<Result<u64, ChangeError>>,
        applied: oneshot::Sender<bool>,
    },
    /// `applied` hears whether the entry at the proposal's index turned out
    /// to be it.
    Propose { command: String, reply: oneshot::Sender<Result<u64, ProposeError>>, applied: oneshot::Sender<bool> },
    SnapshotFailed(NodeId),
    /// The applier saved a snapshot up to this index.
    Snapshotted(u64),
}

enum Apply {
    Entries(Vec<LogEntry>),
    /// Load the snapshot just installed, which ends at this index, with
    /// this membership.
    Restore(u64, Membership),
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
    /// Held while the snapshot files are replaced or read together, by the
    /// writer (this node's snapshots), the Raft loop (the leader's) and
    /// snapshot sending.
    snapshot_file: Mutex<()>,
    /// Where nodes can be reached: the members' urls, and the urls senders
    /// gave for themselves (a node may write to us before we know it).
    urls: Mutex<HashMap<NodeId, String>>,
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
        let initial = Membership::of_voters(config.members.clone());
        if !config.members.is_empty() && !config.members.iter().any(|m| m.id == config.id) {
            let ids: Vec<&NodeId> = config.members.iter().map(|m| &m.id).collect();
            return Err(io::Error::other(format!("node {} is not among the members {ids:?}", config.id)));
        }
        // The membership as of the snapshot, else the one the node starts in.
        let mut snapshot = (0, 0);
        let mut snapshot_members = initial;
        if let Some(header) = loaded.snapshot {
            snapshot = (header.last_index, header.last_term);
            load_snapshot(&state, &loaded.store.snapshot_path(), header.last_index, true)?;
            if let Some(members) = header.members {
                snapshot_members = members;
            }
        }
        let raft = Raft::new(
            config.id.clone(),
            config.raft.clone(),
            loaded.hard,
            snapshot,
            snapshot_members.clone(),
            loaded.entries,
            rand::random(),
        );
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
            members: raft.members().clone(),
            member: raft.members().contains(&config.id),
            voter: raft.is_voter(),
        };
        let urls = raft.members().all().filter(|m| !m.url.is_empty()).map(|m| (m.id.clone(), m.url.clone())).collect();
        let cluster = Arc::new(Cluster {
            config,
            events,
            status: Mutex::new(status),
            applied: watch::channel(snapshot.0).0,
            waiters: Mutex::new(HashMap::new()),
            snapshot_file: Mutex::new(()),
            urls: Mutex::new(urls),
            client,
        });
        let (apply_tx, apply_rx) = std::sync::mpsc::channel();
        let (write_tx, write_rx) = std::sync::mpsc::channel();
        let snapshot_path = loaded.store.snapshot_path();
        {
            let (cluster, state, path) = (cluster.clone(), state.clone(), snapshot_path.clone());
            std::thread::spawn(move || apply_loop(cluster, state, apply_rx, write_tx, snapshot.0, snapshot_members, path));
        }
        {
            let cluster = cluster.clone();
            std::thread::spawn(move || write_loop(cluster, write_rx, snapshot_path));
        }
        tokio::spawn(raft_loop(cluster.clone(), raft, loaded.store, events_rx, apply_tx));
        state.cluster.set(cluster.clone()).map_err(|_| io::Error::other("cluster already started"))?;
        Ok(cluster)
    }

    pub fn status(&self) -> Status {
        let mut status = self.status.lock().unwrap().clone();
        status.applied_index = *self.applied.borrow();
        status
    }

    fn url_of(&self, id: &NodeId) -> Option<String> {
        self.urls.lock().unwrap().get(id).cloned()
    }

    fn learn_url(&self, id: &NodeId, url: &str) {
        if !url.is_empty() {
            self.urls.lock().unwrap().insert(id.clone(), url.to_owned());
        }
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
                    match self.forward(&url, &text).await {
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

    /// Makes one membership change on this node, which must lead, and waits
    /// until it has committed and applied. Retries while a previous change
    /// is still committing.
    async fn change_here(&self, change: MembershipChange, deadline: Instant) -> Result<(), OpError> {
        loop {
            let (reply, response) = oneshot::channel();
            let (tell, applied) = oneshot::channel();
            let _ = self.events.send(Event::ChangeMembership { change: change.clone(), reply, applied: tell });
            match response.await {
                Ok(Ok(_)) => {
                    return match applied.await {
                        Ok(true) => Ok(()),
                        // Another leader's entry took its place.
                        _ => Err(OpError::NotLeader),
                    };
                }
                Ok(Err(ChangeError::Later)) => {
                    if Instant::now() >= deadline {
                        return Err(OpError::Api(unavailable("the previous membership change is still committing")));
                    }
                    tokio::time::sleep(self.config.tick * 2).await;
                }
                Ok(Err(ChangeError::NotLeader)) | Err(_) => return Err(OpError::NotLeader),
                Ok(Err(ChangeError::Invalid(message))) => return Err(OpError::Api(ApiError::bad_request(message))),
            }
        }
    }

    /// Adds `member` as a learner, waits until it has caught up with the
    /// log, then makes it a voter. Run on the leader.
    async fn add_here(&self, member: Member, deadline: Instant) -> Result<Value, OpError> {
        let members = self.status().members;
        if members.is_voter(&member.id) {
            let conflict = ApiError { status: StatusCode::CONFLICT, message: format!("{} is already a voter", member.id) };
            return Err(OpError::Api(conflict));
        }
        if !members.is_learner(&member.id) {
            self.change_here(MembershipChange::AddLearner(member.clone()), deadline).await?;
        }
        // The learner is caught up when it has acknowledged part of the log
        // and is within a few entries of the end.
        loop {
            let status = self.status();
            if status.role != Role::Leader {
                return Err(OpError::NotLeader);
            }
            let matched = status.followers.get(&member.id).copied().unwrap_or(0);
            if matched > 0 && matched + CATCH_UP_ENTRIES >= status.last_index {
                break;
            }
            if Instant::now() >= deadline {
                let message = format!(
                    "{} is a learner but has not caught up (it has entry {matched} of {}); is it running with --join? \
                     Repeat the request to continue",
                    member.id, status.last_index
                );
                return Err(OpError::Api(ApiError { status: StatusCode::GATEWAY_TIMEOUT, message }));
            }
            tokio::time::sleep(self.config.tick * 2).await;
        }
        self.change_here(MembershipChange::Promote(member.id.clone()), deadline).await?;
        Ok(json!({ "id": member.id, "members": self.status().members }))
    }

    /// Removes a node. Run on the leader.
    async fn remove_here(&self, id: NodeId, deadline: Instant) -> Result<Value, OpError> {
        self.change_here(MembershipChange::Remove(id.clone()), deadline).await?;
        Ok(json!({ "id": id, "members": self.status().members }))
    }

    /// Runs a membership operation on the leader: here if this node leads,
    /// else by passing it to the leader. `forwarded` requests come from
    /// another node and are not passed on again.
    async fn membership_op(&self, op: LeaderOp, forwarded: bool) -> Result<Value, ApiError> {
        let deadline = Instant::now() + self.config.member_timeout;
        loop {
            let status = self.status();
            if status.role == Role::Leader {
                let result = match &op {
                    LeaderOp::Add(member) => self.add_here(member.clone(), deadline).await,
                    LeaderOp::Remove(id) => self.remove_here(id.clone(), deadline).await,
                };
                match result {
                    Ok(value) => return Ok(value),
                    Err(OpError::Api(e)) => return Err(e),
                    Err(OpError::NotLeader) if forwarded => return Err(not_the_leader(None)),
                    Err(OpError::NotLeader) => {}
                }
            } else if forwarded {
                return Err(not_the_leader(status.leader));
            } else if let Some(url) = status.leader.as_ref().and_then(|l| self.url_of(l)) {
                match self.forward_op(&url, &op).await {
                    Ok(value) => return Ok(value),
                    Err(e) if e.status == StatusCode::MISDIRECTED_REQUEST => {}
                    Err(e) => return Err(e),
                }
            }
            if Instant::now() >= deadline {
                return Err(unavailable("no leader to make the change"));
            }
            tokio::time::sleep(self.config.tick * 2).await;
        }
    }

    async fn forward_op(&self, url: &str, op: &LeaderOp) -> Result<Value, ApiError> {
        let request = match op {
            LeaderOp::Add(member) => self
                .client
                .post(format!("{url}/1/cluster/members?forwarded=1"))
                .body(json!({ "id": member.id, "url": member.url }).to_string()),
            LeaderOp::Remove(id) => self.client.delete(format!("{url}/1/cluster/members/{id}?forwarded=1")),
        };
        let response = request
            .header("X-Algolia-API-Key", &self.config.admin_key)
            // Longer than the leader waits, so its answer comes through.
            .timeout(self.config.member_timeout + Duration::from_secs(10))
            .send()
            .await
            // Not delivered: the leader is gone, and another is being elected.
            .map_err(|e| if e.is_connect() { not_the_leader(None) } else { unavailable(format!("could not reach the leader: {e}")) })?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            Ok(body)
        } else {
            let message = body["message"].as_str().unwrap_or("the leader refused").to_owned();
            Err(ApiError { status, message })
        }
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

/// A membership operation, which only the leader can carry out.
enum LeaderOp {
    Add(Member),
    Remove(NodeId),
}

enum OpError {
    NotLeader,
    Api(ApiError),
}

fn not_the_leader(leader: Option<NodeId>) -> ApiError {
    ApiError {
        status: StatusCode::MISDIRECTED_REQUEST,
        message: format!("not the leader; the leader is {}", leader.as_deref().unwrap_or("unknown")),
    }
}

/// Owns the Raft state machine and its storage: feeds it events and ticks,
/// saves what it says to save, then sends and applies.
async fn raft_loop(
    cluster: Arc<Cluster>,
    mut raft: Raft,
    mut store: RaftStore,
    mut events: mpsc::UnboundedReceiver<Event>,
    apply: std::sync::mpsc::Sender<Apply>,
) {
    let mut peers: HashMap<NodeId, mpsc::UnboundedSender<Message>> = HashMap::new();
    let mut known_members = raft.members().clone();
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
            Event::Snapshot { from, term, last_index, last_term, members, path } => {
                if let Some(old) = incoming.replace(path) {
                    let _ = std::fs::remove_file(RaftStore::image_of(&old));
                    let _ = std::fs::remove_file(old);
                }
                let members = members.unwrap_or_else(|| raft.members().clone());
                raft.step(&from, Message::InstallSnapshot { term, last_index, last_term, members });
            }
            Event::ChangeMembership { change, reply, applied } => {
                let result = change.apply(raft.members()).map_err(ChangeError::Invalid).and_then(|new| {
                    raft.propose_membership(new).map_err(|e| match e {
                        MembershipError::NotLeader(_) => ChangeError::NotLeader,
                        MembershipError::Pending | MembershipError::NotReady => ChangeError::Later,
                        MembershipError::Invalid(message) => ChangeError::Invalid(message),
                    })
                });
                let result = result.map(|(index, term)| {
                    cluster.waiters.lock().unwrap().insert(index, (term, applied));
                    index
                });
                let _ = reply.send(result);
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
                // The leader's image, if it sent one; an older own image
                // would not match anyway.
                let (image, own_image) = (RaftStore::image_of(&path), RaftStore::image_of(&store.snapshot_path()));
                if image.exists() {
                    std::fs::rename(&image, &own_image)?;
                } else {
                    let _ = std::fs::remove_file(&own_image);
                }
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

        // New members' addresses, before anything is sent to them.
        if *raft.members() != known_members {
            known_members = raft.members().clone();
            for member in known_members.all() {
                cluster.learn_url(&member.id, &member.url);
            }
        }

        // 2. Load a received snapshot (before any entry after it applies).
        if let Some((index, _, members)) = ready.restore {
            let _ = apply.send(Apply::Restore(index, members));
        }

        // 3. Send.
        for (to, message) in ready.messages {
            if let Message::InstallSnapshot { term, .. } = message {
                tokio::spawn(send_snapshot(cluster.clone(), to, term, store.snapshot_path()));
            } else {
                let peer = peers.entry(to.clone()).or_insert_with(|| {
                    let (tx, rx) = mpsc::unbounded_channel();
                    tokio::spawn(send_to_peer(cluster.clone(), to, rx));
                    tx
                });
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
        status.members = known_members.clone();
        status.member = known_members.contains(&cluster.config.id);
        status.voter = raft.is_voter();
    }
}

/// Loads the snapshot at `path` (ending at entry `expected`) into `state`:
/// from its image when there is a matching one, which needs no
/// re-indexing; otherwise from the snapshot itself. With `make_image`, a
/// missing or unusable image is written afresh, so the next start is fast.
fn load_snapshot(state: &AppState, path: &Path, expected: u64, make_image: bool) -> io::Result<()> {
    let started = Instant::now();
    let image = RaftStore::image_of(path);
    let from_image = match RaftStore::read_snapshot(&image) {
        Ok((header, bytes)) if header.last_index == expected => match state.import_images(&bytes) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("cluster: {e}; loading the snapshot instead");
                false
            }
        },
        Ok(_) => false,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => {
            eprintln!("cluster: {e}; loading the snapshot instead");
            false
        }
    };
    if !from_image {
        let (header, bytes) = RaftStore::read_snapshot(path)?;
        assert_eq!(header.last_index, expected, "the snapshot file is the one expected");
        state.import_state(&bytes).map_err(|e| io::Error::other(e.to_string()))?;
        if make_image {
            let members = header.members.clone().unwrap_or_default();
            RaftStore::write_snapshot(&image, header.last_index, header.last_term, &members, &state.export_images())?;
        }
    }
    let how = if from_image { "image" } else { "records" };
    println!("cluster: loaded the snapshot up to entry {expected} from its {how} in {:.0} ms", started.elapsed().as_secs_f64() * 1000.0);
    Ok(())
}

/// A snapshot for the writer to serialize and save: a copy of the state
/// as of an entry; `with_records` false saves only an image (to replace a
/// missing one for the snapshot in place).
struct SnapshotJob {
    last_index: u64,
    last_term: u64,
    /// The membership as of `last_index`.
    members: Membership,
    view: StateView,
    with_records: bool,
}

/// Applies committed entries in order, and snapshots now and then: it
/// only copies the state, and the writer thread serializes and saves the
/// copy, so applying pauses as briefly as possible.
fn apply_loop(
    cluster: Arc<Cluster>,
    state: Arc<AppState>,
    entries: std::sync::mpsc::Receiver<Apply>,
    writer: std::sync::mpsc::Sender<SnapshotJob>,
    mut applied: u64,
    mut applied_members: Membership,
    snapshot_path: PathBuf,
) {
    let mut last_snapshot = applied;
    let mut applied_term = 0;
    for work in entries {
        match work {
            Apply::Restore(expected, members) => {
                if let Err(e) = load_snapshot(&state, &snapshot_path, expected, false) {
                    eprintln!("cluster: cannot load the leader's snapshot: {e}; stopping");
                    std::process::exit(1);
                }
                applied = expected;
                applied_members = members;
                applied_term = RaftStore::read_snapshot_term(&snapshot_path).unwrap_or(0);
                last_snapshot = applied;
                if !RaftStore::image_of(&snapshot_path).exists() {
                    let members = applied_members.clone();
                    let job = SnapshotJob { last_index: applied, last_term: applied_term, members, view: state.view(), with_records: false };
                    let _ = writer.send(job);
                }
            }
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
                    if let Some(members) = entry.members {
                        applied_members = members;
                    }
                    cluster.applied(entry.index, entry.term);
                }
            }
        }
        cluster.applied.send_replace(applied);
        if applied >= last_snapshot + cluster.config.snapshot_every {
            let started = Instant::now();
            let members = applied_members.clone();
            let job = SnapshotJob { last_index: applied, last_term: applied_term, members, view: state.view(), with_records: true };
            println!(
                "cluster: state copied for the snapshot of entry {applied} in {:.0} ms (writes waited meanwhile)",
                started.elapsed().as_secs_f64() * 1000.0
            );
            last_snapshot = applied;
            let _ = writer.send(job);
        }
    }
}

/// Saves snapshots (and their images), then lets the Raft loop drop the
/// log entries they hold.
fn write_loop(cluster: Arc<Cluster>, jobs: std::sync::mpsc::Receiver<SnapshotJob>, snapshot_path: PathBuf) {
    let image_path = RaftStore::image_of(&snapshot_path);
    for job in jobs {
        let started = Instant::now();
        let (index, term) = (job.last_index, job.last_term);
        // Serialized and written under temporary names first: only the
        // renames below hold the lock the Raft loop also takes.
        let prepared = (|| -> io::Result<(Option<PathBuf>, PathBuf)> {
            let records = match job.with_records {
                true => Some(RaftStore::prepare_snapshot(&snapshot_path, index, term, &job.members, &job.view.records())?),
                false => None,
            };
            let image = RaftStore::prepare_snapshot(&image_path, index, term, &job.members, &job.view.images())?;
            Ok((records, image))
        })();
        drop(job.view);
        let (records, image) = match prepared {
            Ok(files) => files,
            Err(e) => {
                eprintln!("cluster: snapshot failed: {e}");
                continue;
            }
        };
        let installed = (|| -> io::Result<bool> {
            let _file = cluster.snapshot_file.lock().unwrap();
            // A newer snapshot from the leader may be in place, waiting to
            // be loaded: never replace it with an older one.
            let current = RaftStore::snapshot_index(&snapshot_path)?.unwrap_or(0);
            let wanted = match &records {
                Some(_) => current < index,
                // Only an image: for the snapshot in place.
                None => current == index,
            };
            if !wanted {
                return Ok(false);
            }
            if let Some(records) = &records {
                RaftStore::install(records, &snapshot_path)?;
            }
            RaftStore::install(&image, &image_path)?;
            Ok(true)
        })();
        for leftover in records.iter().chain([&image]) {
            let _ = std::fs::remove_file(leftover);
        }
        match installed {
            Ok(true) => {
                println!("cluster: snapshot of entry {index} saved in {:.0} ms", started.elapsed().as_secs_f64() * 1000.0);
                if records.is_some() {
                    let _ = cluster.events.send(Event::Snapshotted(index));
                }
            }
            Ok(false) => {}
            Err(e) => eprintln!("cluster: snapshot failed: {e}"),
        }
    }
}

/// Sends a peer its messages in order, a batch per request. Undeliverable
/// messages are dropped: Raft sends again.
async fn send_to_peer(cluster: Arc<Cluster>, peer: NodeId, mut queue: mpsc::UnboundedReceiver<Message>) {
    while let Some(first) = queue.recv().await {
        let mut batch = vec![first];
        while batch.len() < 256 {
            match queue.try_recv() {
                Ok(m) => batch.push(m),
                Err(_) => break,
            }
        }
        // Where the peer is can change, and is not known to a node that has
        // not heard of it yet: then the messages are dropped, and Raft sends
        // them again.
        let Some(url) = cluster.url_of(&peer) else { continue };
        let url = format!("{url}/1/cluster/messages");
        let own_url = cluster.url_of(&cluster.config.id);
        let body = json!({ "from": cluster.config.id, "url": own_url, "messages": batch }).to_string();
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

/// Sends this node's current snapshot to a follower that is too far behind,
/// with its image when it has one for the same entry: the snapshot's bytes,
/// then the image's (`snapshotLength` says where they split).
async fn send_snapshot(cluster: Arc<Cluster>, peer: NodeId, term: u64, path: PathBuf) {
    let result: Result<(), String> = async {
        let url = cluster.url_of(&peer).ok_or("unknown peer")?;
        let files = {
            let cluster = cluster.clone();
            tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, Vec<u8>), String> {
                // Read together, so the image matches the snapshot.
                let _file = cluster.snapshot_file.lock().unwrap();
                let snapshot = std::fs::read(&path).map_err(|e| e.to_string())?;
                let image = std::fs::read(RaftStore::image_of(&path)).unwrap_or_default();
                Ok((snapshot, image))
            })
        };
        let (mut body, image) = files.await.map_err(|e| e.to_string())??;
        // The files may be newer than the message that asked for them: send
        // what they hold.
        let (header, _) = RaftStore::parse_snapshot(&body)?;
        let snapshot_length = body.len();
        if RaftStore::parse_snapshot(&image).is_ok_and(|(h, _)| h.last_index == header.last_index) {
            body.extend_from_slice(&image);
        }
        let query = format!(
            "from={}&term={term}&lastIndex={}&lastTerm={}&snapshotLength={snapshot_length}",
            cluster.config.id, header.last_index, header.last_term
        );
        let response = cluster
            .client
            .post(format!("{url}/1/cluster/snapshot?{query}"))
            .header("X-Algolia-API-Key", &cluster.config.admin_key)
            .timeout(Duration::from_secs(600))
            .body(body)
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
        .route("/1/cluster/members", get(list_members).post(add_member))
        .route("/1/cluster/members/{id}", delete(remove_member))
        // Batches of large writes and whole snapshots.
        .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024 * 1024))
}

fn cluster_of(state: &AppState) -> Result<&Arc<Cluster>, ApiError> {
    state.cluster.get().ok_or_else(|| ApiError::not_found("this server is not part of a cluster"))
}

#[derive(Deserialize)]
struct Messages {
    from: NodeId,
    /// Where the sender can be reached, if it knows.
    #[serde(default)]
    url: Option<String>,
    messages: Vec<Message>,
}

async fn receive_messages(State(state): State<Arc<AppState>>, body: Bytes) -> Result<Json<Value>, ApiError> {
    let cluster = cluster_of(&state)?;
    let Messages { from, url, messages } =
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("bad messages: {e}")))?;
    if let Some(url) = url {
        cluster.learn_url(&from, &url);
    }
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
    /// Bytes of the snapshot; an image may follow (absent: the whole body).
    snapshot_length: Option<usize>,
}

async fn receive_snapshot(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SnapshotQuery>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let cluster = cluster_of(&state)?;
    let split = q.snapshot_length.unwrap_or(body.len()).min(body.len());
    let (snapshot, image) = (body.slice(..split), body.slice(split..));
    let (header, _) = RaftStore::parse_snapshot(&snapshot).map_err(ApiError::bad_request)?;
    let members = header.members.clone();
    if (header.last_index, header.last_term) != (q.last_index, q.last_term) {
        return Err(ApiError::bad_request("snapshot header does not match the request"));
    }
    // A damaged or mismatched image is dropped: the snapshot alone will do.
    let image_ok = RaftStore::parse_snapshot(&image).is_ok_and(|(h, _)| h.last_index == header.last_index);
    let path = cluster.config.dir.join(format!("snapshot.incoming-{:016x}", rand::random::<u64>()));
    let written = tokio::task::spawn_blocking({
        let path = path.clone();
        move || -> io::Result<()> {
            use std::io::Write;
            let save = |to: &Path, bytes: &[u8]| -> io::Result<()> {
                let mut file = std::fs::File::create(to)?;
                file.write_all(bytes)?;
                file.sync_all()
            };
            if image_ok {
                save(&RaftStore::image_of(&path), &image)?;
            }
            save(&path, &snapshot)
        }
    })
    .await
    .expect("write task");
    written.map_err(|e| ApiError::storage(e.into()))?;
    let event =
        Event::Snapshot { from: q.from, term: q.term, last_index: q.last_index, last_term: q.last_term, members, path };
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

async fn list_members(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let status = cluster_of(&state)?.status();
    Ok(Json(json!({
        "voters": status.members.voters,
        "learners": status.members.learners,
        "leader": status.leader,
        "matched": status.followers,
        "lastIndex": status.last_index,
    })))
}

#[derive(Deserialize)]
struct MemberQuery {
    #[serde(default)]
    forwarded: Option<String>,
}

#[derive(Deserialize)]
struct NewMember {
    id: NodeId,
    url: String,
}

/// Adds a node; returns once it is a voter.
async fn add_member(
    State(state): State<Arc<AppState>>,
    Query(q): Query<MemberQuery>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let cluster = cluster_of(&state)?;
    let NewMember { id, url } =
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("body needs an id and a url: {e}")))?;
    let url = url.trim().trim_end_matches('/').to_owned();
    if id.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(ApiError::bad_request("a node needs an id and an http(s) url"));
    }
    let value = cluster.membership_op(LeaderOp::Add(Member { id, url }), q.forwarded.is_some()).await?;
    Ok(Json(value))
}

async fn remove_member(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<MemberQuery>,
) -> Result<Json<Value>, ApiError> {
    let value = cluster_of(&state)?.membership_op(LeaderOp::Remove(id), q.forwarded.is_some()).await?;
    Ok(Json(value))
}
