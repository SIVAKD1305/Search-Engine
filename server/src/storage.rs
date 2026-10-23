//! On-disk storage for one index: a snapshot plus a write-ahead log.
//!
//! An index directory holds one generation `N`:
//!
//! - `snapshot-N.jsonl`: a header line `{"version":1,"settings":{...}}`, one
//!   record per line, and a trailer `{"end":<record count>}`.
//! - `wal-N.log`: every change since that snapshot, one per line, as
//!   `<crc32 in hex> <json>`.
//!
//! A write is acknowledged only after its log lines are fsynced. On startup
//! the snapshot is loaded and its log replayed. A damaged *last* log line is
//! what a crash in the middle of a write leaves behind, so it is cut off;
//! damage anywhere else is reported as corruption instead of guessed around.
//!
//! When the log grows past a limit, the next generation's snapshot is
//! written to a temporary file and renamed into place (an atomic step), an
//! empty log is started, and the old generation is deleted.

use engine::index::{Index, Settings};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

pub const SNAPSHOT_AFTER_ENTRIES: u64 = 10_000;
pub const SNAPSHOT_AFTER_BYTES: u64 = 64 * 1024 * 1024;
const FORMAT_VERSION: u64 = 1;

/// One logged change. Partial updates are logged as the full resulting
/// record, so replaying the log never depends on anything but the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Entry {
    Put { record: Map<String, Value> },
    Delete {
        #[serde(rename = "objectID")]
        object_id: String,
    },
    Clear,
    Settings { settings: Settings },
}

impl Entry {
    pub fn apply(&self, index: &mut Index) {
        match self {
            Entry::Put { record } => {
                index.add(Value::Object(record.clone())).expect("logged records have an objectID");
            }
            Entry::Delete { object_id } => {
                index.delete(object_id);
            }
            Entry::Clear => index.clear(),
            Entry::Settings { settings } => index.set_settings(settings.clone()),
        }
    }
}

#[derive(Debug)]
pub enum StorageError {
    Io(io::Error),
    Corrupt(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Io(e) => write!(f, "storage I/O error: {e}"),
            StorageError::Corrupt(msg) => write!(f, "corrupt storage: {msg}"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<io::Error> for StorageError {
    fn from(e: io::Error) -> Self {
        StorageError::Io(e)
    }
}

fn corrupt(path: &Path, msg: impl fmt::Display) -> StorageError {
    StorageError::Corrupt(format!("{}: {msg}", path.display()))
}

fn snapshot_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("snapshot-{generation}.jsonl"))
}

fn wal_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("wal-{generation}.log"))
}

/// Makes renames and new files in `dir` durable.
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn write_snapshot(dir: &Path, generation: u64, index: &Index) -> Result<(), StorageError> {
    let path = snapshot_path(dir, generation);
    let tmp = path.with_extension("jsonl.tmp");
    let file = File::create(&tmp)?;
    let mut out = BufWriter::new(file);
    let header = json!({ "version": FORMAT_VERSION, "settings": index.settings() });
    serde_json::to_writer(&mut out, &header).map_err(io::Error::from)?;
    out.write_all(b"\n")?;
    let mut count = 0u64;
    for (_, record) in index.store().iter() {
        serde_json::to_writer(&mut out, record).map_err(io::Error::from)?;
        out.write_all(b"\n")?;
        count += 1;
    }
    serde_json::to_writer(&mut out, &json!({ "end": count })).map_err(io::Error::from)?;
    out.write_all(b"\n")?;
    let file = out.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    fs::rename(&tmp, &path)?;
    sync_dir(dir)?;
    Ok(())
}

fn read_snapshot(path: &Path) -> Result<Index, StorageError> {
    let mut lines = BufReader::new(File::open(path)?).lines();
    let header: Value = match lines.next() {
        Some(line) => serde_json::from_str(&line?).map_err(|e| corrupt(path, format!("bad header: {e}")))?,
        None => return Err(corrupt(path, "empty snapshot")),
    };
    if header["version"].as_u64() != Some(FORMAT_VERSION) {
        return Err(corrupt(path, format!("unsupported version {}", header["version"])));
    }
    let settings: Settings = serde_json::from_value(header["settings"].clone())
        .map_err(|e| corrupt(path, format!("bad settings: {e}")))?;
    let mut index = Index::new(settings);
    let mut count = 0u64;
    for (n, line) in lines.enumerate() {
        let line_no = n + 2;
        let value: Value =
            serde_json::from_str(&line?).map_err(|e| corrupt(path, format!("line {line_no}: {e}")))?;
        if value.get("objectID").is_some() {
            index.add(value).map_err(|e| corrupt(path, format!("line {line_no}: {e}")))?;
            count += 1;
        } else if value["end"].as_u64() == Some(count) {
            return Ok(index);
        } else {
            return Err(corrupt(path, format!("line {line_no}: bad trailer {value}")));
        }
    }
    Err(corrupt(path, "missing trailer (incomplete snapshot)"))
}

fn encode(entry: &Entry) -> String {
    let body = serde_json::to_string(entry).expect("entries serialize");
    format!("{:08x} {body}\n", crc32fast::hash(body.as_bytes()))
}

fn decode(line: &[u8]) -> Option<Entry> {
    let line = std::str::from_utf8(line).ok()?;
    let (crc, body) = line.split_once(' ')?;
    let crc = u32::from_str_radix(crc, 16).ok()?;
    (crc32fast::hash(body.as_bytes()) == crc).then(|| serde_json::from_str(body).ok())?
}

/// Applies the log to `index`. Returns (entries, bytes) kept.
fn replay_wal(path: &Path, index: &mut Index) -> Result<(u64, u64), StorageError> {
    let data = fs::read(path)?;
    let mut offset = 0usize;
    let mut entries = 0u64;
    let mut line_no = 0usize;
    while offset < data.len() {
        line_no += 1;
        let rest = &data[offset..];
        let (line, complete) = match rest.iter().position(|&b| b == b'\n') {
            Some(end) => (&rest[..end], true),
            None => (rest, false),
        };
        let next = offset + line.len() + usize::from(complete);
        let is_last = next >= data.len();
        match decode(line).filter(|_| complete) {
            Some(entry) => {
                entry.apply(index);
                entries += 1;
                offset = next;
            }
            None if is_last => {
                // A crash during the last write: drop the partial line.
                eprintln!("{}: dropping incomplete last entry (line {line_no})", path.display());
                OpenOptions::new().write(true).open(path)?.set_len(offset as u64)?;
                return Ok((entries, offset as u64));
            }
            None => return Err(corrupt(path, format!("line {line_no} is damaged"))),
        }
    }
    Ok((entries, offset as u64))
}

fn open_wal(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

pub struct IndexStore {
    dir: PathBuf,
    generation: u64,
    wal: File,
    wal_entries: u64,
    wal_bytes: u64,
}

impl IndexStore {
    /// Creates storage for a new index, starting from its current contents.
    pub fn create(dir: &Path, index: &Index) -> Result<Self, StorageError> {
        fs::create_dir_all(dir)?;
        if let Some(parent) = dir.parent() {
            sync_dir(parent)?;
        }
        write_snapshot(dir, 1, index)?;
        let wal = open_wal(&wal_path(dir, 1))?;
        sync_dir(dir)?;
        Ok(IndexStore { dir: dir.to_owned(), generation: 1, wal, wal_entries: 0, wal_bytes: 0 })
    }

    /// Loads an index from its directory.
    pub fn open(dir: &Path) -> Result<(Index, Self), StorageError> {
        let mut generations = Vec::new();
        for entry in fs::read_dir(dir)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if name.ends_with(".tmp") {
                // A snapshot that was never renamed into place.
                fs::remove_file(dir.join(&name))?;
            } else if let Some(n) = name.strip_prefix("snapshot-").and_then(|s| s.strip_suffix(".jsonl")) {
                generations.push(n.parse::<u64>().map_err(|_| corrupt(dir, format!("unexpected file {name}")))?);
            }
        }
        let generation = *generations.iter().max().ok_or_else(|| corrupt(dir, "no snapshot"))?;
        let mut index = read_snapshot(&snapshot_path(dir, generation))?;
        let wal = wal_path(dir, generation);
        let (wal_entries, wal_bytes) = if wal.exists() { replay_wal(&wal, &mut index)? } else { (0, 0) };
        let file = open_wal(&wal)?;
        // Older generations are fully contained in this one.
        for old in generations.into_iter().filter(|&g| g < generation) {
            let _ = fs::remove_file(snapshot_path(dir, old));
            let _ = fs::remove_file(wal_path(dir, old));
        }
        sync_dir(dir)?;
        Ok((index, IndexStore { dir: dir.to_owned(), generation, wal: file, wal_entries, wal_bytes }))
    }

    /// Appends entries and waits until they are on disk.
    pub fn append(&mut self, entries: &[Entry]) -> Result<(), StorageError> {
        let text: String = entries.iter().map(encode).collect();
        self.wal.write_all(text.as_bytes())?;
        self.wal.sync_data()?;
        self.wal_entries += entries.len() as u64;
        self.wal_bytes += text.len() as u64;
        Ok(())
    }

    pub fn needs_snapshot(&self) -> bool {
        self.wal_entries >= SNAPSHOT_AFTER_ENTRIES || self.wal_bytes >= SNAPSHOT_AFTER_BYTES
    }

    /// Writes `index` as the next generation and drops the old log.
    pub fn snapshot(&mut self, index: &Index) -> Result<(), StorageError> {
        let next = self.generation + 1;
        write_snapshot(&self.dir, next, index)?;
        let wal = open_wal(&wal_path(&self.dir, next))?;
        sync_dir(&self.dir)?;
        let old = std::mem::replace(&mut self.wal, wal);
        drop(old);
        let _ = fs::remove_file(snapshot_path(&self.dir, self.generation));
        let _ = fs::remove_file(wal_path(&self.dir, self.generation));
        self.generation = next;
        self.wal_entries = 0;
        self.wal_bytes = 0;
        Ok(())
    }

    /// Deletes the index's directory.
    pub fn destroy(self) -> io::Result<()> {
        let dir = self.dir.clone();
        drop(self);
        fs::remove_dir_all(&dir)?;
        if let Some(parent) = dir.parent() {
            sync_dir(parent)?;
        }
        Ok(())
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Directory of the index `name` under the data directory.
pub fn index_dir(root: &Path, name: &str) -> PathBuf {
    root.join("indexes").join(name)
}

/// Opens every index stored under `root`.
pub fn open_all(root: &Path) -> Result<Vec<(String, Index, IndexStore)>, StorageError> {
    let indexes = root.join("indexes");
    fs::create_dir_all(&indexes)?;
    let mut opened = Vec::new();
    for entry in fs::read_dir(&indexes)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let (index, store) = IndexStore::open(&entry.path())?;
        opened.push((name, index, store));
    }
    opened.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(opened)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn put(record: Value) -> Entry {
        Entry::Put { record: record.as_object().unwrap().clone() }
    }

    fn names(index: &Index) -> Vec<String> {
        index.store().iter().map(|(_, r)| r["name"].as_str().unwrap_or("").to_owned()).collect()
    }

    /// Applies entries to memory and disk, as the server does.
    fn write(index: &mut Index, store: &mut IndexStore, entries: &[Entry]) {
        store.append(entries).unwrap();
        for e in entries {
            e.apply(index);
        }
    }

    fn settings() -> Settings {
        serde_json::from_value(json!({"searchableAttributes": ["name"], "customRanking": ["desc(rating)"]})).unwrap()
    }

    #[test]
    fn changes_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::new(settings());
        let mut store = IndexStore::create(dir.path(), &index).unwrap();
        write(&mut index, &mut store, &[
            put(json!({"objectID": "1", "name": "Kettle"})),
            put(json!({"objectID": "2", "name": "Toaster"})),
            Entry::Delete { object_id: "1".into() },
            put(json!({"objectID": "3", "name": "Teapot"})),
        ]);
        drop(store);

        let (reopened, store) = IndexStore::open(dir.path()).unwrap();
        assert_eq!(names(&reopened), ["Toaster", "Teapot"]);
        assert_eq!(reopened.settings().custom_ranking.len(), 1);
        assert_eq!((store.wal_entries, store.generation()), (4, 1));
    }

    #[test]
    fn settings_and_clear_are_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::new(Settings::default());
        let mut store = IndexStore::create(dir.path(), &index).unwrap();
        write(&mut index, &mut store, &[
            put(json!({"objectID": "1", "name": "Kettle", "brand": "Orbix"})),
            Entry::Clear,
            put(json!({"objectID": "2", "name": "Lamp", "brand": "Orbix"})),
            Entry::Settings { settings: settings() },
        ]);
        drop(store);
        let (reopened, _) = IndexStore::open(dir.path()).unwrap();
        assert_eq!(names(&reopened), ["Lamp"]);
        // "brand" is no longer searchable after the settings change.
        assert!(reopened.word("orbix").is_none());
        assert!(reopened.word("lamp").is_some());
    }

    #[test]
    fn torn_last_entry_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::new(Settings::default());
        let mut store = IndexStore::create(dir.path(), &index).unwrap();
        write(&mut index, &mut store, &[put(json!({"objectID": "1", "name": "Kettle"}))]);
        drop(store);
        // Simulate a crash half-way through writing the next entry.
        let wal = wal_path(dir.path(), 1);
        let good_len = fs::metadata(&wal).unwrap().len();
        let half = &encode(&put(json!({"objectID": "2", "name": "Lamp"})))[..20];
        OpenOptions::new().append(true).open(&wal).unwrap().write_all(half.as_bytes()).unwrap();

        let (reopened, mut store) = IndexStore::open(dir.path()).unwrap();
        assert_eq!(names(&reopened), ["Kettle"]);
        assert_eq!(fs::metadata(&wal).unwrap().len(), good_len);
        // Writing continues cleanly after the cut.
        store.append(&[put(json!({"objectID": "3", "name": "Desk"}))]).unwrap();
        drop(store);
        let (reopened, _) = IndexStore::open(dir.path()).unwrap();
        assert_eq!(names(&reopened), ["Kettle", "Desk"]);
    }

    #[test]
    fn damage_before_the_end_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::new(Settings::default());
        let mut store = IndexStore::create(dir.path(), &index).unwrap();
        write(&mut index, &mut store, &[
            put(json!({"objectID": "1", "name": "Kettle"})),
            put(json!({"objectID": "2", "name": "Lamp"})),
        ]);
        drop(store);
        let wal = wal_path(dir.path(), 1);
        let mut bytes = fs::read(&wal).unwrap();
        let pos = bytes.iter().position(|&b| b == b'K').unwrap();
        bytes[pos] = b'X'; // flips the checksum of line 1
        fs::write(&wal, bytes).unwrap();
        match IndexStore::open(dir.path()) {
            Err(StorageError::Corrupt(msg)) => assert!(msg.ends_with("line 1 is damaged"), "{msg}"),
            other => panic!("expected corruption, got {:?}", other.err()),
        }
    }

    #[test]
    fn snapshot_starts_a_new_generation() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::new(settings());
        let mut store = IndexStore::create(dir.path(), &index).unwrap();
        write(&mut index, &mut store, &[put(json!({"objectID": "1", "name": "Kettle", "rating": 2}))]);
        store.snapshot(&index).unwrap();
        write(&mut index, &mut store, &[put(json!({"objectID": "2", "name": "Lamp", "rating": 5}))]);
        assert_eq!(store.generation(), 2);
        let mut files: Vec<String> =
            fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        files.sort();
        assert_eq!(files, ["snapshot-2.jsonl", "wal-2.log"]);
        drop(store);
        let (reopened, store) = IndexStore::open(dir.path()).unwrap();
        assert_eq!(names(&reopened), ["Kettle", "Lamp"]);
        assert_eq!(store.wal_entries, 1);
    }

    #[test]
    fn incomplete_snapshot_files_are_ignored_or_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::new(Settings::default());
        drop(IndexStore::create(dir.path(), &index).unwrap());
        // A leftover temporary file from a crash during snapshotting.
        fs::write(dir.path().join("snapshot-2.jsonl.tmp"), "{\"version\":1").unwrap();
        IndexStore::open(dir.path()).unwrap();
        assert!(!dir.path().join("snapshot-2.jsonl.tmp").exists());
        // A snapshot without its trailer is never accepted.
        fs::write(snapshot_path(dir.path(), 1), "{\"version\":1,\"settings\":{}}\n").unwrap();
        assert!(matches!(IndexStore::open(dir.path()), Err(StorageError::Corrupt(_))));
    }

    #[test]
    fn open_all_and_destroy() {
        let root = tempfile::tempdir().unwrap();
        for name in ["movies", "products"] {
            let mut index = Index::new(Settings::default());
            index.add(json!({"objectID": "1", "name": name})).unwrap();
            IndexStore::create(&index_dir(root.path(), name), &index).unwrap();
        }
        let opened = open_all(root.path()).unwrap();
        assert_eq!(opened.iter().map(|(n, _, _)| n.as_str()).collect::<Vec<_>>(), ["movies", "products"]);
        let (_, _, store) = opened.into_iter().next().unwrap();
        store.destroy().unwrap();
        assert_eq!(open_all(root.path()).unwrap().len(), 1);
    }
}
