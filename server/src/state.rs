//! Server state: the indexes, their storage, and every way they change.
//!
//! All changes go through one path per index: take the index's write log,
//! give the changes the next sequence numbers, make them durable, apply them
//! in memory, and keep them in a short in-memory history that replicas read.
//! A primary produces changes from client requests (`commit`); a replica
//! receives numbered changes from its primary (`apply_replicated`) or a whole
//! index (`install_index`).

use crate::ApiError;
use crate::keys::{ApiKey, KeyStore, RateLimiter};
use crate::storage::{self, Entry, IndexStore, Logged, SnapshotData, StorageError};
use axum::http::StatusCode;
use engine::index::{Index, Settings};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;

/// Changes kept in memory per index for replicas that are catching up.
/// A replica further behind downloads a snapshot instead.
pub const DEFAULT_RECENT_LIMIT: usize = 100_000;

/// The index's position in its history.
pub struct WriteLog {
    store: Option<IndexStore>,
    seq: u64,
    epoch: u64,
    recent: VecDeque<Logged>,
}

/// One index. Writers take `log` first, so writes to an index are
/// serialized while searches keep reading `index`; the index lock is held
/// for writing only while changes are applied in memory.
pub struct IndexEntry {
    pub index: RwLock<Index>,
    log: Mutex<WriteLog>,
    /// Client writes waiting to be committed together (group commit).
    queue: Mutex<CommitQueue>,
    committed: Condvar,
}

/// Writes that arrive while a commit is running wait here; the next leader
/// commits all of them with one fsync.
#[derive(Default)]
struct CommitQueue {
    pending: Vec<(u64, Vec<Op>)>,
    next_ticket: u64,
    /// Every ticket up to this one has been committed (or has failed).
    done_through: u64,
    leader_active: bool,
    /// Tickets whose commit failed, with the reason.
    failed: HashMap<u64, String>,
}

pub type SharedIndex = Arc<IndexEntry>;

impl IndexEntry {
    fn new(index: Index, store: Option<IndexStore>, seq: u64, epoch: u64, recent: Vec<Logged>) -> SharedIndex {
        let log = WriteLog { store, seq, epoch, recent: recent.into() };
        let queue = CommitQueue { next_ticket: 1, ..CommitQueue::default() };
        Arc::new(IndexEntry {
            index: RwLock::new(index),
            log: Mutex::new(log),
            queue: Mutex::new(queue),
            committed: Condvar::new(),
        })
    }

    /// (sequence number, epoch)
    pub fn position(&self) -> (u64, u64) {
        let log = self.log.lock().unwrap();
        (log.seq, log.epoch)
    }
}

/// A requested change, before it is resolved against the stored records.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Op {
    Put(String, Map<String, Value>),
    Partial { id: String, changes: Map<String, Value>, create: bool },
    Delete(String),
    Clear,
    Settings(Settings),
    /// Settings to change, merged into the settings current when applied
    /// (so concurrent changes of different settings are all kept). The
    /// result must have been checked to be valid.
    MergeSettings(Map<String, Value>),
}

/// The settings `current` would become with `changes`.
pub fn merge_settings(current: &Settings, changes: &Map<String, Value>) -> Result<Settings, String> {
    let mut merged = serde_json::to_value(current).expect("settings serialize");
    for (k, v) in changes {
        merged[k] = v.clone();
    }
    serde_json::from_value(merged).map_err(|e| format!("invalid settings: {e}"))
}

/// A change agreed on by a cluster (see `cluster`). Every node applies the
/// same commands in the same order, resolving partial updates as it
/// applies them, so every node computes the same records, sequence numbers
/// and epochs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Command {
    Write { index: String, create: bool, ops: Vec<Op> },
    DeleteIndex { index: String },
    PutKey { key: ApiKey },
    DeleteKey { value: String },
}

/// The epoch of an index a cluster created at log entry `log_index`: the
/// same on every node, different for every re-creation.
fn cluster_epoch(log_index: u64) -> u64 {
    // splitmix64
    let mut z = log_index.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (z ^ (z >> 31)) | 1
}

/// Every index and key at one moment (`AppState::view`).
pub struct StateView {
    keys: Vec<ApiKey>,
    /// Name, sequence number, epoch and the index.
    indexes: Vec<(String, u64, u64, Index)>,
}

impl StateView {
    /// The state in `AppState::export_state`'s format.
    pub fn records(&self) -> Vec<u8> {
        let mut parts = Vec::new();
        let mut indexes = Vec::new();
        for (name, seq, epoch, index) in &self.indexes {
            let mut bytes = Vec::new();
            storage::write_snapshot_to(&mut bytes, index, *seq, *epoch).expect("writing to memory");
            indexes.push((name.clone(), bytes.len()));
            parts.push(bytes);
        }
        join(&ExportHeader { keys: self.keys.clone(), indexes }, parts)
    }

    /// The state in `AppState::export_images`'s format.
    pub fn images(&self) -> Vec<u8> {
        let mut parts = Vec::new();
        let mut indexes = Vec::new();
        for (name, seq, epoch, index) in &self.indexes {
            let mut bytes = Vec::new();
            index.write_image(&mut bytes).expect("writing to memory");
            indexes.push((name.clone(), *seq, *epoch, bytes.len()));
            parts.push(bytes);
        }
        join(&ImagesHeader { keys: self.keys.clone(), indexes }, parts)
    }
}

/// A JSON header line followed by the parts.
fn join(header: &impl Serialize, parts: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = serde_json::to_vec(header).expect("serializable");
    out.push(b'\n');
    for part in parts {
        out.extend_from_slice(&part);
    }
    out
}

/// The header of exported index images (`AppState::export_images`).
#[derive(Serialize, Deserialize)]
struct ImagesHeader {
    keys: Vec<ApiKey>,
    /// Name, sequence number, epoch and image length of each index.
    indexes: Vec<(String, u64, u64, usize)>,
}

/// The header of an exported state (`AppState::export_state`).
#[derive(Serialize, Deserialize)]
struct ExportHeader {
    keys: Vec<ApiKey>,
    /// Name and byte length of each index's snapshot, in order.
    indexes: Vec<(String, usize)>,
}

/// Turns operations into log entries. Partial updates become full records,
/// looking first at earlier operations of the same request.
fn resolve(index: &Index, ops: Vec<Op>) -> Vec<Entry> {
    let mut pending: HashMap<String, Option<Map<String, Value>>> = HashMap::new();
    let mut cleared = false;
    let mut settings_now: Option<Settings> = None;
    let mut entries = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            Op::Put(id, mut record) => {
                record.insert("objectID".into(), Value::String(id.clone()));
                pending.insert(id, Some(record.clone()));
                entries.push(Entry::Put { record });
            }
            Op::Partial { id, changes, create } => {
                let current = match pending.get(&id) {
                    Some(record) => record.clone(),
                    None if cleared => None,
                    None => index.get_object(&id),
                };
                let mut record = match current {
                    Some(record) => record,
                    None if create => Map::new(),
                    None => continue,
                };
                record.extend(changes);
                record.insert("objectID".into(), Value::String(id.clone()));
                pending.insert(id, Some(record.clone()));
                entries.push(Entry::Put { record });
            }
            Op::Delete(id) => {
                pending.insert(id.clone(), None);
                entries.push(Entry::Delete { object_id: id });
            }
            Op::Clear => {
                pending.clear();
                cleared = true;
                entries.push(Entry::Clear);
            }
            Op::Settings(settings) => {
                settings_now = Some(settings.clone());
                entries.push(Entry::Settings { settings });
            }
            Op::MergeSettings(changes) => {
                let current = settings_now.as_ref().unwrap_or_else(|| index.settings());
                if let Ok(settings) = merge_settings(current, &changes) {
                    settings_now = Some(settings.clone());
                    entries.push(Entry::Settings { settings });
                }
            }
        }
    }
    entries
}

/// Why changes from the primary could not be applied.
#[derive(Debug)]
pub enum ReplicationError {
    /// The local copy no longer follows the primary's history: download the
    /// whole index again.
    Diverged(String),
    Storage(StorageError),
}

impl std::fmt::Display for ReplicationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplicationError::Diverged(msg) => write!(f, "replica diverged: {msg}"),
            ReplicationError::Storage(e) => write!(f, "{e}"),
        }
    }
}

/// A slice of an index's history, for a replica.
pub struct LogSlice {
    pub epoch: u64,
    /// The primary's latest sequence number.
    pub seq: u64,
    pub entries: Vec<Logged>,
}

/// What a replica knows about its primary.
#[derive(Debug, Default, Clone)]
pub struct ReplicaStatus {
    pub last_contact: Option<Instant>,
    pub last_error: Option<String>,
    /// Latest sequence number seen on the primary, per index.
    pub primary_seq: HashMap<String, u64>,
}

pub struct AppState {
    indexes: RwLock<HashMap<String, SharedIndex>>,
    /// Held while an index is created, replaced or deleted, including the
    /// disk work, so that deleting `x` and creating a new `x` at the same
    /// time cannot remove the new index's files.
    lifecycle: Mutex<()>,
    /// `None` keeps everything in memory.
    data_dir: Option<PathBuf>,
    pub(crate) admin_key: String,
    pub(crate) search_key: String,
    /// Keys created with `/1/keys`.
    pub keys: KeyStore,
    pub(crate) limiter: RateLimiter,
    /// Take the client address from `X-Forwarded-For` (behind a proxy).
    pub(crate) trust_proxy: bool,
    /// Set when this server is a node of a Raft cluster.
    pub cluster: OnceLock<Arc<crate::cluster::Cluster>>,
    /// Random per process, so objectIDs made on different nodes differ.
    object_salt: u32,
    next_task: AtomicU64,
    next_object: AtomicU64,
    /// Counts every change on any index; replicas wait for it to move.
    changes: watch::Sender<u64>,
    /// Set on a read replica: the primary's URL.
    primary: Option<String>,
    recent_limit: usize,
    pub(crate) replica_status: Mutex<ReplicaStatus>,
}

impl AppState {
    /// A server that keeps its data in memory only.
    pub fn new(admin_key: String, search_key: String) -> Self {
        assert!(!admin_key.is_empty() && !search_key.is_empty(), "API keys must not be empty");
        AppState {
            indexes: RwLock::new(HashMap::new()),
            lifecycle: Mutex::new(()),
            data_dir: None,
            admin_key,
            search_key,
            keys: KeyStore::in_memory(),
            limiter: RateLimiter::default(),
            trust_proxy: false,
            cluster: OnceLock::new(),
            object_salt: rand::random(),
            next_task: AtomicU64::new(1),
            next_object: AtomicU64::new(1),
            changes: watch::channel(0).0,
            primary: None,
            recent_limit: DEFAULT_RECENT_LIMIT,
            replica_status: Mutex::new(ReplicaStatus::default()),
        }
    }

    /// A server that stores its data under `dir`, loading what is there.
    pub fn open(admin_key: String, search_key: String, dir: &Path) -> Result<Self, StorageError> {
        let mut state = AppState::new(admin_key, search_key);
        state.data_dir = Some(dir.to_owned());
        std::fs::create_dir_all(dir)?;
        state.keys = KeyStore::open(dir.join("keys.json"))?;
        for (name, loaded) in storage::open_all(dir)? {
            let entry = IndexEntry::new(loaded.index, Some(loaded.store), loaded.seq, loaded.epoch, loaded.recent);
            state.indexes.get_mut().unwrap().insert(name, entry);
        }
        Ok(state)
    }

    /// Makes this server a read replica of `primary_url`.
    pub fn as_replica_of(mut self, primary_url: impl Into<String>) -> Self {
        self.primary = Some(primary_url.into().trim_end_matches('/').to_owned());
        self
    }

    /// Trust `X-Forwarded-For` for the client address (rate limits,
    /// `restrictSources`); only behind a proxy that sets it.
    pub fn trusting_proxy(mut self, trust: bool) -> Self {
        self.trust_proxy = trust;
        self
    }

    /// How many recent changes to keep per index for replicas.
    pub fn with_recent_limit(mut self, limit: usize) -> Self {
        self.recent_limit = limit.max(1);
        self
    }

    pub fn primary(&self) -> Option<&str> {
        self.primary.as_deref()
    }

    pub fn index_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.indexes.read().unwrap().keys().cloned().collect();
        names.sort();
        names
    }

    pub fn indexes(&self) -> Vec<(String, SharedIndex)> {
        let mut all: Vec<_> = self.indexes.read().unwrap().iter().map(|(n, e)| (n.clone(), e.clone())).collect();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        all
    }

    pub fn index(&self, name: &str) -> Result<SharedIndex, ApiError> {
        self.indexes
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("Index {name} does not exist")))
    }

    /// Replicas only accept changes from their primary.
    pub(crate) fn ensure_writable(&self) -> Result<(), ApiError> {
        match &self.primary {
            None => Ok(()),
            Some(primary) => Err(ApiError {
                status: StatusCode::FORBIDDEN,
                message: format!("this server is a read replica; send writes to {primary}"),
            }),
        }
    }

    fn new_store(&self, name: &str, index: &Index, seq: u64, epoch: u64) -> Result<Option<IndexStore>, StorageError> {
        match &self.data_dir {
            Some(dir) => Ok(Some(IndexStore::create(&storage::index_dir(dir, name), index, seq, epoch)?)),
            None => Ok(None),
        }
    }

    /// Writes create the index if needed, as in Algolia.
    pub fn index_or_create(&self, name: &str) -> Result<SharedIndex, ApiError> {
        self.ensure_writable()?;
        crate::validate_index_name(name)?;
        if let Some(entry) = self.indexes.read().unwrap().get(name) {
            return Ok(entry.clone());
        }
        let _lifecycle = self.lifecycle.lock().unwrap();
        let mut indexes = self.indexes.write().unwrap();
        if let Some(entry) = indexes.get(name) {
            return Ok(entry.clone());
        }
        let index = Index::new(Settings::default());
        let epoch = storage::new_epoch();
        let store = self.new_store(name, &index, 0, epoch).map_err(ApiError::storage)?;
        let entry = IndexEntry::new(index, store, 0, epoch, Vec::new());
        indexes.insert(name.to_owned(), entry.clone());
        drop(indexes);
        self.changed();
        Ok(entry)
    }

    /// Adds or replaces an index with a pre-built one (used at startup).
    pub fn insert_index(&self, name: &str, index: Index) -> Result<(), StorageError> {
        crate::validate_index_name(name).map_err(|e| StorageError::Corrupt(e.message))?;
        self.install(name, SnapshotData { index, seq: 0, epoch: Some(storage::new_epoch()) })
    }

    /// Replaces an index with a copy received from the primary.
    pub fn install_index(&self, name: &str, data: SnapshotData) -> Result<(), StorageError> {
        crate::validate_index_name(name).map_err(|e| StorageError::Corrupt(e.message))?;
        self.install(name, data)
    }

    fn install(&self, name: &str, data: SnapshotData) -> Result<(), StorageError> {
        let _lifecycle = self.lifecycle.lock().unwrap();
        let epoch = data.epoch.unwrap_or_else(storage::new_epoch);
        if let Some(old) = self.indexes.write().unwrap().remove(name)
            && let Some(store) = old.log.lock().unwrap().store.take()
        {
            store.destroy()?;
        }
        let store = self.new_store(name, &data.index, data.seq, epoch)?;
        let entry = IndexEntry::new(data.index, store, data.seq, epoch, Vec::new());
        self.indexes.write().unwrap().insert(name.to_owned(), entry);
        self.changed();
        Ok(())
    }

    /// Deletes an index and its files.
    pub fn delete_index(&self, name: &str) -> Result<(), ApiError> {
        self.ensure_writable()?;
        self.remove_index(name).map_err(|e| ApiError::storage(e.into()))?.then_some(()).ok_or_else(|| {
            ApiError::not_found(format!("Index {name} does not exist"))
        })
    }

    /// Removes an index; returns false if it did not exist.
    pub fn remove_index(&self, name: &str) -> std::io::Result<bool> {
        let _lifecycle = self.lifecycle.lock().unwrap();
        let Some(entry) = self.indexes.write().unwrap().remove(name) else { return Ok(false) };
        if let Some(store) = entry.log.lock().unwrap().store.take() {
            store.destroy()?;
        }
        self.changed();
        Ok(true)
    }

    /// Applies client changes: numbers them, makes them durable, applies
    /// them. Blocks until they are on disk.
    ///
    /// Group commit: a writer that finds no commit running becomes the
    /// leader and commits every write queued so far with one fsync; writers
    /// arriving meanwhile queue up for the next round.
    pub fn commit(&self, entry: &SharedIndex, ops: Vec<Op>) -> Result<(), ApiError> {
        self.ensure_writable()?;
        let mut queue = entry.queue.lock().unwrap();
        let ticket = queue.next_ticket;
        queue.next_ticket += 1;
        queue.pending.push((ticket, ops));
        loop {
            if queue.done_through >= ticket {
                return match queue.failed.remove(&ticket) {
                    None => Ok(()),
                    Some(message) => Err(ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message }),
                };
            }
            if queue.leader_active {
                queue = entry.committed.wait(queue).unwrap();
                continue;
            }
            queue.leader_active = true;
            let batch = std::mem::take(&mut queue.pending);
            drop(queue);
            let last = batch.last().map_or(0, |(t, _)| *t);
            let tickets: Vec<u64> = batch.iter().map(|(t, _)| *t).collect();
            let result = self.commit_batch(entry, batch.into_iter().flat_map(|(_, ops)| ops).collect());
            queue = entry.queue.lock().unwrap();
            if let Err(e) = result {
                for t in tickets {
                    queue.failed.insert(t, e.message.clone());
                }
            }
            queue.done_through = last;
            queue.leader_active = false;
            entry.committed.notify_all();
        }
    }

    /// Commits the operations of several requests, in order, as one write.
    fn commit_batch(&self, entry: &SharedIndex, ops: Vec<Op>) -> Result<(), ApiError> {
        self.write_ops(entry, ops).map_err(ApiError::storage)
    }

    /// Resolves `ops` and writes them as the index's next changes.
    fn write_ops(&self, entry: &IndexEntry, ops: Vec<Op>) -> Result<(), StorageError> {
        let mut log = entry.log.lock().unwrap();
        // One resolution for the whole batch, so a request sees the changes
        // of the requests queued before it.
        let entries = resolve(&entry.index.read().unwrap(), ops);
        let first = log.seq + 1;
        let logged: Vec<Logged> =
            entries.into_iter().enumerate().map(|(i, entry)| Logged { seq: first + i as u64, entry }).collect();
        self.write(entry, &mut log, logged)
    }

    /// Applies a command a cluster agreed on, from log entry `log_index`.
    /// Commands that no longer make sense (a write to an index deleted in
    /// the meantime) do nothing, the same way on every node.
    pub fn apply_command(&self, command: Command, log_index: u64) -> Result<(), StorageError> {
        match command {
            Command::Write { index, create, ops } => {
                let existing = self.indexes.read().unwrap().get(&index).cloned();
                let entry = match existing {
                    Some(entry) => entry,
                    None if create && crate::validate_index_name(&index).is_ok() => {
                        let entry = IndexEntry::new(Index::new(Settings::default()), None, 0, cluster_epoch(log_index), Vec::new());
                        self.indexes.write().unwrap().insert(index, entry.clone());
                        entry
                    }
                    None => return Ok(()),
                };
                self.write_ops(&entry, ops)?;
                self.changed();
            }
            Command::DeleteIndex { index } => {
                self.remove_index(&index)?;
            }
            Command::PutKey { key } => {
                self.keys.put(key)?;
                self.changed();
            }
            Command::DeleteKey { value } => {
                self.keys.remove(&value)?;
                self.changed();
            }
        }
        Ok(())
    }

    /// Every index (with its position) and every key, for a cluster
    /// snapshot: a JSON header line, then each index's snapshot.
    pub fn export_state(&self) -> Vec<u8> {
        self.view().records()
    }

    /// The same as `export_state`, with each index as an image (see
    /// `Index::write_image`): loads without re-indexing, but only into the
    /// same engine version.
    pub fn export_images(&self) -> Vec<u8> {
        self.view().images()
    }

    /// A copy of every index and key as they are now. Copying is much
    /// faster than serializing, so writes wait only for the copy.
    pub fn view(&self) -> StateView {
        let indexes = self
            .indexes()
            .into_iter()
            .map(|(name, entry)| {
                let log = entry.log.lock().unwrap();
                let index = entry.index.read().unwrap().clone();
                (name, log.seq, log.epoch, index)
            })
            .collect();
        StateView { keys: self.keys.list(), indexes }
    }

    /// Replaces every index and key with exported images. Fails, changing
    /// nothing, if any image cannot be read (another engine version).
    pub fn import_images(&self, bytes: &[u8]) -> Result<(), StorageError> {
        let corrupt = |m: String| StorageError::Corrupt(format!("cluster images: {m}"));
        let newline = bytes.iter().position(|&b| b == b'\n').ok_or_else(|| corrupt("no header".into()))?;
        let header: ImagesHeader = serde_json::from_slice(&bytes[..newline]).map_err(|e| corrupt(e.to_string()))?;
        let mut offset = newline + 1;
        // Read every image before installing any.
        let mut loaded = Vec::new();
        for (name, seq, epoch, len) in header.indexes {
            let part = bytes.get(offset..offset + len).ok_or_else(|| corrupt(format!("{name} is cut short")))?;
            offset += len;
            let index = Index::read_image(part).map_err(|e| corrupt(format!("{name}: {e}")))?;
            loaded.push((name, SnapshotData { index, seq, epoch: Some(epoch) }));
        }
        let names: Vec<String> = loaded.iter().map(|(n, _)| n.clone()).collect();
        for (name, data) in loaded {
            self.install_index(&name, data)?;
        }
        for name in self.index_names() {
            if !names.contains(&name) {
                self.remove_index(&name)?;
            }
        }
        self.keys.replace_all(header.keys)?;
        self.changed();
        Ok(())
    }

    /// Replaces every index and key with an exported state.
    pub fn import_state(&self, bytes: &[u8]) -> Result<(), StorageError> {
        let corrupt = |m: String| StorageError::Corrupt(format!("cluster snapshot: {m}"));
        let newline = bytes.iter().position(|&b| b == b'\n').ok_or_else(|| corrupt("no header".into()))?;
        let header: ExportHeader = serde_json::from_slice(&bytes[..newline]).map_err(|e| corrupt(e.to_string()))?;
        let mut offset = newline + 1;
        let mut names = Vec::new();
        for (name, len) in header.indexes {
            let part = bytes.get(offset..offset + len).ok_or_else(|| corrupt(format!("{name} is cut short")))?;
            offset += len;
            let data = storage::read_snapshot_from(part, Path::new(&name))?;
            self.install_index(&name, data)?;
            names.push(name);
        }
        for name in self.index_names() {
            if !names.contains(&name) {
                self.remove_index(&name)?;
            }
        }
        self.keys.replace_all(header.keys)?;
        self.changed();
        Ok(())
    }

    /// Applies numbered changes from the primary. Changes already applied
    /// are skipped, so fetching the same range twice is harmless.
    pub fn apply_replicated(&self, name: &str, epoch: u64, logged: Vec<Logged>) -> Result<(), ReplicationError> {
        let entry = self.index(name).map_err(|e| ReplicationError::Diverged(e.message))?;
        let mut log = entry.log.lock().unwrap();
        if log.epoch != epoch {
            return Err(ReplicationError::Diverged(format!("{name}: epoch changed")));
        }
        let fresh: Vec<Logged> = logged.into_iter().filter(|l| l.seq > log.seq).collect();
        if let Some(first) = fresh.first()
            && first.seq != log.seq + 1
        {
            return Err(ReplicationError::Diverged(format!("{name}: expected {}, got {}", log.seq + 1, first.seq)));
        }
        if fresh.windows(2).any(|w| w[1].seq != w[0].seq + 1) {
            return Err(ReplicationError::Diverged(format!("{name}: gap in received changes")));
        }
        self.write(&entry, &mut log, fresh).map_err(ReplicationError::Storage)
    }

    fn write(&self, entry: &IndexEntry, log: &mut WriteLog, logged: Vec<Logged>) -> Result<(), StorageError> {
        let Some(last) = logged.last().map(|l| l.seq) else { return Ok(()) };
        if let Some(store) = log.store.as_mut() {
            store.append(&logged)?;
        }
        {
            let mut index = entry.index.write().unwrap();
            for l in &logged {
                l.entry.apply(&mut index);
            }
        }
        log.seq = last;
        log.recent.extend(logged);
        while log.recent.len() > self.recent_limit {
            log.recent.pop_front();
        }
        let (seq, epoch) = (log.seq, log.epoch);
        if let Some(store) = log.store.as_mut()
            && store.needs_snapshot()
        {
            // The log still holds everything, so a failed snapshot loses nothing.
            if let Err(e) = store.snapshot(&entry.index.read().unwrap(), seq, epoch) {
                eprintln!("snapshot failed: {e}");
            }
        }
        self.changed();
        Ok(())
    }

    /// Writes a fresh snapshot (and image) of every index whose log holds
    /// changes, so the next start reads images and replays nothing. Called
    /// on shutdown; writes wait meanwhile, searches do not.
    pub fn checkpoint(&self) -> Result<(), StorageError> {
        for (_, entry) in self.indexes() {
            let mut log = entry.log.lock().unwrap();
            let (seq, epoch) = (log.seq, log.epoch);
            if let Some(store) = log.store.as_mut()
                && store.has_log_entries()
            {
                store.snapshot(&entry.index.read().unwrap(), seq, epoch)?;
            }
        }
        Ok(())
    }

    /// Changes with sequence numbers from `from`, at most `limit` of them.
    /// `None` when they are no longer kept in memory.
    pub fn log_since(&self, name: &str, from: u64, limit: usize) -> Result<Option<LogSlice>, ApiError> {
        let entry = self.index(name)?;
        let log = entry.log.lock().unwrap();
        let first_kept = log.recent.front().map_or(log.seq + 1, |l| l.seq);
        if from < first_kept && from <= log.seq {
            return Ok(None);
        }
        let entries = log.recent.iter().filter(|l| l.seq >= from).take(limit).cloned().collect();
        Ok(Some(LogSlice { epoch: log.epoch, seq: log.seq, entries }))
    }

    /// The whole index as snapshot bytes, consistent with one sequence number.
    pub fn snapshot_bytes(&self, name: &str) -> Result<Vec<u8>, ApiError> {
        let entry = self.index(name)?;
        let log = entry.log.lock().unwrap();
        let index = entry.index.read().unwrap();
        let mut bytes = Vec::new();
        storage::write_snapshot_to(&mut bytes, &index, log.seq, log.epoch).expect("writing to memory");
        Ok(bytes)
    }

    pub(crate) fn changed(&self) {
        self.changes.send_modify(|v| *v += 1);
    }

    /// The change counter, and a way to wait for it to move.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub fn task(&self) -> u64 {
        self.next_task.fetch_add(1, Ordering::Relaxed)
    }

    /// An objectID for records added without one.
    pub fn new_object_id(&self) -> String {
        let n = self.next_object.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        format!("{nanos:x}{:08x}{n:x}", self.object_salt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Barrier;

    #[test]
    fn concurrent_writes_are_grouped_and_all_kept() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::open("a".into(), "s".into(), dir.path()).unwrap());
        let entry = state.index_or_create("group").unwrap();
        let threads: Vec<_> = (0..16)
            .map(|t| {
                let (state, entry) = (state.clone(), entry.clone());
                std::thread::spawn(move || {
                    for i in 0..50 {
                        let record = json!({"t": t, "i": i}).as_object().unwrap().clone();
                        state.commit(&entry, vec![Op::Put(format!("{t}-{i}"), record)]).unwrap();
                        // Every thread also adds its own field to one shared record.
                        let field = json!({ format!("f{t}"): i }).as_object().unwrap().clone();
                        let op = Op::Partial { id: "shared".into(), changes: field, create: true };
                        state.commit(&entry, vec![op]).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let check = |index: &Index| {
            assert_eq!(index.stats().records, 16 * 50 + 1);
            let shared = index.get_object("shared").unwrap();
            for t in 0..16 {
                // No partial update was lost: each thread's last value is there.
                assert_eq!(shared[&format!("f{t}")], 49, "field f{t}");
            }
        };
        check(&entry.index.read().unwrap());
        assert_eq!(entry.position().0, 16 * 50 * 2);
        drop((entry, state));
        // The log replays to the same state, with contiguous sequence numbers.
        let reopened = AppState::open("a".into(), "s".into(), dir.path()).unwrap();
        let entry = reopened.index("group").unwrap();
        check(&entry.index.read().unwrap());
        assert_eq!(entry.position().0, 16 * 50 * 2);
    }

    #[test]
    fn deleting_and_recreating_at_once_keeps_the_new_index_files() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::open("a".into(), "s".into(), dir.path()).unwrap());
        for round in 0..200 {
            let entry = state.index_or_create("race").unwrap();
            state.commit(&entry, vec![Op::Put("old".into(), Map::new())]).unwrap();
            let barrier = Arc::new(Barrier::new(2));
            let deleter = {
                let (state, barrier) = (state.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    state.remove_index("race").unwrap();
                })
            };
            barrier.wait();
            let created = state.index_or_create("race").unwrap();
            let record = json!({"v": round}).as_object().unwrap().clone();
            state.commit(&created, vec![Op::Put("new".into(), record)]).unwrap();
            deleter.join().unwrap();
            // An index that is live in memory must still have its files.
            if state.index("race").is_ok() {
                assert!(dir.path().join("indexes/race").exists(), "round {round}: files of a live index were deleted");
            }
        }
    }

    #[test]
    fn exported_images_load_the_same_state_or_nothing() {
        let state = AppState::new("a".into(), "s".into());
        let write = |index: &str, ops: Vec<Op>, at: u64| {
            state.apply_command(Command::Write { index: index.into(), create: true, ops }, at).unwrap();
        };
        let record = |id: &str, name: &str| Op::Put(id.into(), serde_json::json!({"name": name}).as_object().unwrap().clone());
        write("products", vec![record("1", "Kettle"), record("2", "Lamp")], 1);
        write("movies", vec![record("m", "Alien")], 2);
        write("products", vec![Op::Delete("1".into())], 3);
        let images = state.export_images();

        let copy = AppState::new("a".into(), "s".into());
        copy.apply_command(Command::Write { index: "stale".into(), create: true, ops: vec![] }, 1).unwrap();
        copy.import_images(&images).unwrap();
        assert_eq!(copy.index_names(), ["movies", "products"]);
        assert_eq!(copy.export_state(), state.export_state());
        assert_eq!(copy.index("products").unwrap().position(), state.index("products").unwrap().position());

        // A damaged image is refused before anything changes.
        let other = AppState::new("a".into(), "s".into());
        other.apply_command(Command::Write { index: "kept".into(), create: true, ops: vec![] }, 1).unwrap();
        let mut damaged = images.clone();
        let last = damaged.len() - 10;
        damaged.truncate(last);
        assert!(other.import_images(&damaged).is_err());
        assert_eq!(other.index_names(), ["kept"]);
    }
}
