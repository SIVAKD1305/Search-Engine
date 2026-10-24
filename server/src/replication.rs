//! Read replicas, like Algolia's Distributed Search Network.
//!
//! One primary accepts writes. Each replica copies the primary's indexes and
//! keeps up with every change, so it can answer searches close to users and
//! keep answering while the primary is down. Replicas refuse client writes.
//!
//! The primary serves (admin key required):
//!
//! - `GET /1/replication/indexes` — each index's sequence number and epoch,
//!   plus a change counter (`version`).
//! - `GET /1/replication/indexes/{index}/log?from=N` — changes from N on,
//!   or 410 Gone when they are no longer kept in memory.
//! - `GET /1/replication/indexes/{index}/snapshot` — the whole index.
//! - `GET /1/replication/wait?version=V` — returns as soon as the change
//!   counter passes V (long polling), or after a timeout.
//! - `GET /1/replication/keys` — the API keys made with `/1/keys`, so that
//!   a replica accepts the same keys (and secured keys made from them).
//!
//! A replica repeats: copy the keys; list the indexes; fetch missing
//! changes, or the whole index when it is new, re-created (other epoch) or
//! too far behind; drop indexes the primary no longer has; then wait for the
//! next change.
//! Because a replica serves the same endpoints, replicas can be chained.

use crate::state::{AppState, ReplicationError};
use crate::storage::{self, Logged};
use crate::ApiError;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Changes sent per log request.
pub const LOG_PAGE: usize = 10_000;
/// Longest a wait request is held open.
pub const MAX_WAIT: Duration = Duration::from_secs(30);

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/1/replication/indexes", get(list))
        .route("/1/replication/indexes/{index}/log", get(log))
        .route("/1/replication/indexes/{index}/snapshot", get(snapshot))
        .route("/1/replication/wait", get(wait))
        .route("/1/replication/status", get(status))
        .route("/1/replication/keys", get(keys))
}

async fn keys(State(state): State<Arc<AppState>>) -> Json<Vec<crate::keys::ApiKey>> {
    Json(state.keys.list())
}

#[derive(Debug, Serialize, Deserialize)]
struct Position {
    name: String,
    seq: u64,
    epoch: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Listing {
    version: u64,
    indexes: Vec<Position>,
}

async fn list(State(state): State<Arc<AppState>>) -> Json<Listing> {
    // Read the counter first: a change made while listing moves it past
    // `version`, so the replica's next wait returns at once.
    let version = *state.changes().borrow();
    let indexes = state
        .indexes()
        .into_iter()
        .map(|(name, entry)| {
            let (seq, epoch) = entry.position();
            Position { name, seq, epoch }
        })
        .collect();
    Json(Listing { version, indexes })
}

#[derive(Deserialize)]
struct LogQuery {
    from: u64,
    limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LogPage {
    epoch: u64,
    seq: u64,
    entries: Vec<Logged>,
}

async fn log(State(state): State<Arc<AppState>>, Path(name): Path<String>, Query(q): Query<LogQuery>) -> Response {
    let limit = q.limit.unwrap_or(LOG_PAGE).clamp(1, LOG_PAGE);
    match state.log_since(&name, q.from, limit) {
        Ok(Some(slice)) => Json(LogPage { epoch: slice.epoch, seq: slice.seq, entries: slice.entries }).into_response(),
        Ok(None) => ApiError { status: StatusCode::GONE, message: "changes are no longer kept; fetch the snapshot".into() }
            .into_response(),
        Err(e) => e.into_response(),
    }
}

async fn snapshot(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let blocking = tokio::task::spawn_blocking(move || state.snapshot_bytes(&name));
    match blocking.await.expect("snapshot task") {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/x-ndjson")], Body::from(bytes)).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WaitQuery {
    version: u64,
    timeout_ms: Option<u64>,
}

async fn wait(State(state): State<Arc<AppState>>, Query(q): Query<WaitQuery>) -> Json<Value> {
    let timeout = Duration::from_millis(q.timeout_ms.unwrap_or(25_000)).min(MAX_WAIT);
    let mut changes = state.changes();
    let _ = tokio::time::timeout(timeout, changes.wait_for(|v| *v > q.version)).await;
    let version = *changes.borrow();
    Json(json!({ "version": version }))
}

async fn status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let replica = state.replica_status.lock().unwrap().clone();
    let indexes: Vec<Value> = state
        .indexes()
        .into_iter()
        .map(|(name, entry)| {
            let (seq, epoch) = entry.position();
            let primary_seq = replica.primary_seq.get(&name).copied();
            json!({
                "name": name,
                "seq": seq,
                "epoch": epoch,
                "primarySeq": primary_seq,
                "lag": primary_seq.map(|p| p.saturating_sub(seq)),
            })
        })
        .collect();
    Json(json!({
        "role": if state.primary().is_some() { "replica" } else { "primary" },
        "primary": state.primary(),
        "secondsSinceContact": replica.last_contact.map(|t| t.elapsed().as_secs_f64()),
        "lastError": replica.last_error,
        "indexes": indexes,
    }))
}

// ---- replica side ----

type Error = Box<dyn std::error::Error + Send + Sync>;

struct Primary {
    url: String,
    key: String,
    client: reqwest::Client,
}

impl Primary {
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client.get(format!("{}{path}", self.url)).header("X-Algolia-API-Key", &self.key)
    }

    async fn json<T: serde::de::DeserializeOwned>(&self, path: &str, timeout: Duration) -> Result<T, Error> {
        let response = self.get(path).timeout(timeout).send().await?.error_for_status()?;
        Ok(response.json().await?)
    }
}

/// Keeps `state` in sync with its primary until the process stops.
pub async fn run_replica(state: Arc<AppState>, admin_key: String) {
    let url = state.primary().expect("replica has a primary").to_owned();
    // Keepalive probes notice a primary that vanished without closing the
    // connection (a network cut) while a wait request is open.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .tcp_keepalive(Duration::from_secs(10))
        .build()
        .expect("HTTP client");
    let primary = Primary { url, key: admin_key, client };
    let mut backoff = Duration::from_millis(250);
    loop {
        let result = match sync(&state, &primary).await {
            Ok(version) => {
                backoff = Duration::from_millis(250);
                record(&state, None);
                let path = format!("/1/replication/wait?version={version}&timeoutMs=25000");
                primary.json::<Value>(&path, MAX_WAIT + Duration::from_secs(10)).await.map(|_| ())
            }
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            eprintln!("replication: {e}; retrying in {backoff:?}");
            record(&state, Some(e.to_string()));
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    }
}

fn record(state: &AppState, error: Option<String>) {
    let mut status = state.replica_status.lock().unwrap();
    if error.is_none() {
        status.last_contact = Some(Instant::now());
    }
    status.last_error = error;
}

/// Brings every index up to date once. Returns the primary's change counter
/// as of the start of the sync.
async fn sync(state: &Arc<AppState>, primary: &Primary) -> Result<u64, Error> {
    let listing: Listing = primary.json("/1/replication/indexes", Duration::from_secs(30)).await?;
    // After the listing, so a key change during the sync wakes the next wait.
    let keys: Vec<crate::keys::ApiKey> = primary.json("/1/replication/keys", Duration::from_secs(30)).await?;
    if state.keys.replace_all(keys)? {
        println!("replication: copied {} API key(s)", state.keys.list().len());
    }
    for remote in &listing.indexes {
        sync_index(state, primary, remote).await?;
        state.replica_status.lock().unwrap().primary_seq.insert(remote.name.clone(), remote.seq);
    }
    let remote: HashSet<&str> = listing.indexes.iter().map(|p| p.name.as_str()).collect();
    for name in state.index_names() {
        if !remote.contains(name.as_str()) {
            state.remove_index(&name)?;
            state.replica_status.lock().unwrap().primary_seq.remove(&name);
        }
    }
    record(state, None);
    Ok(listing.version)
}

async fn sync_index(state: &Arc<AppState>, primary: &Primary, remote: &Position) -> Result<(), Error> {
    loop {
        let local = state.index(&remote.name).ok().map(|e| e.position());
        let seq = match local {
            Some((seq, epoch)) if epoch == remote.epoch && seq >= remote.seq => return Ok(()),
            Some((seq, epoch)) if epoch == remote.epoch => seq,
            _ => return copy_index(state, primary, &remote.name).await,
        };
        let path = format!("/1/replication/indexes/{}/log?from={}", remote.name, seq + 1);
        let response = primary.get(&path).timeout(Duration::from_secs(60)).send().await?;
        if response.status() == StatusCode::GONE {
            return copy_index(state, primary, &remote.name).await;
        }
        let page: LogPage = response.error_for_status()?.json().await?;
        if page.entries.is_empty() {
            return Ok(());
        }
        let (state2, name) = (state.clone(), remote.name.clone());
        let applied = tokio::task::spawn_blocking(move || state2.apply_replicated(&name, page.epoch, page.entries)).await?;
        match applied {
            Ok(()) => {}
            Err(ReplicationError::Diverged(msg)) => {
                eprintln!("replication: {msg}; copying the whole index");
                return copy_index(state, primary, &remote.name).await;
            }
            Err(ReplicationError::Storage(e)) => return Err(e.into()),
        }
    }
}

/// Downloads the whole index and replaces the local copy.
async fn copy_index(state: &Arc<AppState>, primary: &Primary, name: &str) -> Result<(), Error> {
    let path = format!("/1/replication/indexes/{name}/snapshot");
    let response = primary.get(&path).timeout(Duration::from_secs(600)).send().await?;
    if response.status() == StatusCode::NOT_FOUND {
        // Deleted since the listing; the next sync removes it here too.
        return Ok(());
    }
    let bytes = response.error_for_status()?.bytes().await?;
    let (state, name) = (state.clone(), name.to_owned());
    tokio::task::spawn_blocking(move || -> Result<(), Error> {
        let source = std::path::PathBuf::from(format!("{name} from primary"));
        let data = storage::read_snapshot_from(&bytes[..], &source)?;
        println!("replication: copied {name} ({} records, seq {})", data.index.stats().records, data.seq);
        state.install_index(&name, data)?;
        Ok(())
    })
    .await?
}
