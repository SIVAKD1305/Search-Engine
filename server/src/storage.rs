//! On-disk storage for one index: a snapshot plus a write-ahead log.
//!
//! An index directory holds one generation `N`:
//!
//! - `snapshot-N.jsonl`: a header line
//!   `{"version":2,"settings":{...},"seq":S,"epoch":E}`, one record per line,
//!   and a trailer `{"end":<record count>}`.
//! - `wal-N.log`: every change after sequence number `S`, one per line, as
//!   `<crc32 in hex> {"seq":S+1,"entry":{...}}`.
//!
//! Every change has a sequence number (1, 2, 3, ... per index); replicas use
//! it to know what they are missing. The epoch is a random id given to an
//! index when it is created, so a replica can tell a re-created index from
//! the old one.
//!
//! A write is acknowledged only after its log lines are fsynced. On startup
//! the snapshot is loaded and its log replayed. A damaged *last* log line is
//! what a crash in the middle of a write leaves behind, so it is cut off;
//! damage anywhere else is reported as corruption instead of guessed around.
//!
//! When the log grows past a limit, the next generation's snapshot is
//! written to a temporary file and renamed into place (an atomic step), an
//! empty log is started, and the old generation is deleted.
//!
//! Version 1 files (step 9, without sequence numbers) are still read; their
//! entries are numbered from 1 and the index gets a new epoch.
//!
//! Next to each snapshot, `image-N.bin` holds the same index already built
//! (see `Index::write_image`), so startup does not re-index every record:
//! a 100k-record index loads in about a second instead of 13. The image is
//! only a cache: it is checked against a checksum, and when it is missing,
//! damaged or from another engine version, the snapshot is read instead.

use engine::index::{Index, Settings};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

pub const SNAPSHOT_AFTER_ENTRIES: u64 = 10_000;
pub const SNAPSHOT_AFTER_BYTES: u64 = 64 * 1024 * 1024;
const FORMAT_VERSION: u64 = 2;

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

/// An entry with its sequence number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Logged {
    pub seq: u64,
    pub entry: Entry,
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

/// A new random epoch.
pub fn new_epoch() -> u64 {
    rand::random::<u64>() | 1
}

fn snapshot_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("snapshot-{generation}.jsonl"))
}

fn wal_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("wal-{generation}.log"))
}

fn image_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("image-{generation}.bin"))
}

/// Ends every image file, after the checksum and length.
const IMAGE_MAGIC: &[u8; 8] = b"SEIMAGE1";

/// Passes writes through while computing their CRC32.
struct Checksummed<W> {
    inner: W,
    hasher: crc32fast::Hasher,
    len: u64,
}

impl<W: Write> Write for Checksummed<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Writes the image of a snapshot: `seq`, `epoch` (8 bytes each, little
/// endian) and the index, then the CRC32 (4 bytes) and length (8 bytes) of
/// all that, then `IMAGE_MAGIC`. It is not fsynced: a lost or torn image
/// fails its checksum and the snapshot is used.
fn write_image(dir: &Path, generation: u64, index: &Index, seq: u64, epoch: u64) -> io::Result<()> {
    let path = image_path(dir, generation);
    let tmp = path.with_extension("bin.tmp");
    let mut out = Checksummed { inner: BufWriter::new(File::create(&tmp)?), hasher: crc32fast::Hasher::new(), len: 0 };
    out.write_all(&seq.to_le_bytes())?;
    out.write_all(&epoch.to_le_bytes())?;
    index.write_image(&mut out)?;
    let Checksummed { mut inner, hasher, len } = out;
    inner.write_all(&hasher.finalize().to_le_bytes())?;
    inner.write_all(&len.to_le_bytes())?;
    inner.write_all(IMAGE_MAGIC)?;
    inner.flush()?;
    drop(inner);
    fs::rename(&tmp, &path)
}

/// Reads an image written by `write_image`.
fn read_image(path: &Path) -> Result<SnapshotData, StorageError> {
    let data = fs::read(path)?;
    let trailer = 4 + 8 + IMAGE_MAGIC.len();
    if data.len() < 16 + trailer || !data.ends_with(IMAGE_MAGIC) {
        return Err(corrupt(path, "not a complete image"));
    }
    let (body, rest) = data.split_at(data.len() - trailer);
    let crc = u32::from_le_bytes(rest[..4].try_into().unwrap());
    let len = u64::from_le_bytes(rest[4..12].try_into().unwrap());
    if len != body.len() as u64 || crc32fast::hash(body) != crc {
        return Err(corrupt(path, "checksum mismatch"));
    }
    let seq = u64::from_le_bytes(body[..8].try_into().unwrap());
    let epoch = u64::from_le_bytes(body[8..16].try_into().unwrap());
    let index = Index::read_image(&body[16..]).map_err(|e| corrupt(path, e))?;
    Ok(SnapshotData { index, seq, epoch: Some(epoch) })
}

/// Makes renames and new files in `dir` durable.
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Writes a snapshot of `index` at sequence `seq`. Also used to send an
/// index to a replica.
pub fn write_snapshot_to(out: &mut impl Write, index: &Index, seq: u64, epoch: u64) -> io::Result<()> {
    let header = json!({ "version": FORMAT_VERSION, "settings": index.settings(), "seq": seq, "epoch": epoch });
    serde_json::to_writer(&mut *out, &header)?;
    out.write_all(b"\n")?;
    let mut count = 0u64;
    // Records are stored as compact JSON already: copy them as they are.
    for (_, record) in index.store().iter_raw() {
        out.write_all(record.as_bytes())?;
        out.write_all(b"\n")?;
        count += 1;
    }
    serde_json::to_writer(&mut *out, &json!({ "end": count }))?;
    out.write_all(b"\n")
}

fn write_snapshot(dir: &Path, generation: u64, index: &Index, seq: u64, epoch: u64) -> Result<(), StorageError> {
    let path = snapshot_path(dir, generation);
    let tmp = path.with_extension("jsonl.tmp");
    let mut out = BufWriter::new(File::create(&tmp)?);
    write_snapshot_to(&mut out, index, seq, epoch)?;
    let file = out.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    fs::rename(&tmp, &path)?;
    sync_dir(dir)?;
    // The snapshot is safe on disk; the image only speeds up the next start.
    if let Err(e) = write_image(dir, generation, index, seq, epoch) {
        eprintln!("{}: could not write index image: {e}", dir.display());
        let _ = fs::remove_file(image_path(dir, generation).with_extension("bin.tmp"));
    }
    Ok(())
}

/// A snapshot read back: the index, its sequence number, and its epoch
/// (`None` for version 1 files).
pub struct SnapshotData {
    pub index: Index,
    pub seq: u64,
    pub epoch: Option<u64>,
}

/// Reads a snapshot written by `write_snapshot_to`. `source` names it in errors.
pub fn read_snapshot_from(reader: impl BufRead, source: &Path) -> Result<SnapshotData, StorageError> {
    let mut lines = reader.lines();
    let header: Value = match lines.next() {
        Some(line) => serde_json::from_str(&line?).map_err(|e| corrupt(source, format!("bad header: {e}")))?,
        None => return Err(corrupt(source, "empty snapshot")),
    };
    let (seq, epoch) = match header["version"].as_u64() {
        Some(1) => (0, None),
        Some(2) => {
            let seq = header["seq"].as_u64().ok_or_else(|| corrupt(source, "header has no seq"))?;
            let epoch = header["epoch"].as_u64().ok_or_else(|| corrupt(source, "header has no epoch"))?;
            (seq, Some(epoch))
        }
        _ => return Err(corrupt(source, format!("unsupported version {}", header["version"]))),
    };
    let settings: Settings = serde_json::from_value(header["settings"].clone())
        .map_err(|e| corrupt(source, format!("bad settings: {e}")))?;
    let mut index = Index::new(settings);
    let mut count = 0u64;
    for (n, line) in lines.enumerate() {
        let line_no = n + 2;
        let value: Value =
            serde_json::from_str(&line?).map_err(|e| corrupt(source, format!("line {line_no}: {e}")))?;
        if value.get("objectID").is_some() {
            index.add(value).map_err(|e| corrupt(source, format!("line {line_no}: {e}")))?;
            count += 1;
        } else if value["end"].as_u64() == Some(count) {
            return Ok(SnapshotData { index, seq, epoch });
        } else {
            return Err(corrupt(source, format!("line {line_no}: bad trailer {value}")));
        }
    }
    Err(corrupt(source, "missing trailer (incomplete snapshot)"))
}

fn encode(logged: &Logged) -> String {
    let body = serde_json::to_string(logged).expect("entries serialize");
    format!("{:08x} {body}\n", crc32fast::hash(body.as_bytes()))
}

/// A log line's entry and, for version 2 lines, its sequence number.
fn decode(line: &[u8]) -> Option<(Option<u64>, Entry)> {
    let line = std::str::from_utf8(line).ok()?;
    let (crc, body) = line.split_once(' ')?;
    let crc = u32::from_str_radix(crc, 16).ok()?;
    if crc32fast::hash(body.as_bytes()) != crc {
        return None;
    }
    if let Ok(logged) = serde_json::from_str::<Logged>(body) {
        return Some((Some(logged.seq), logged.entry));
    }
    serde_json::from_str::<Entry>(body).ok().map(|entry| (None, entry))
}

/// What replaying a log found.
struct Replayed {
    seq: u64,
    entries: Vec<Logged>,
    bytes: u64,
}

/// Applies the log to `index`, continuing from sequence number `seq`.
fn replay_wal(path: &Path, index: &mut Index, mut seq: u64) -> Result<Replayed, StorageError> {
    let data = fs::read(path)?;
    let mut offset = 0usize;
    let mut entries = Vec::new();
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
            Some((logged_seq, entry)) => {
                let expected = seq + 1;
                if logged_seq.is_some_and(|s| s != expected) {
                    return Err(corrupt(path, format!("line {line_no}: seq {logged_seq:?}, expected {expected}")));
                }
                entry.apply(index);
                seq = expected;
                entries.push(Logged { seq, entry });
                offset = next;
            }
            None if is_last => {
                // A crash during the last write: drop the partial line.
                eprintln!("{}: dropping incomplete last entry (line {line_no})", path.display());
                OpenOptions::new().write(true).open(path)?.set_len(offset as u64)?;
                break;
            }
            None => return Err(corrupt(path, format!("line {line_no} is damaged"))),
        }
    }
    Ok(Replayed { seq, entries, bytes: offset as u64 })
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

/// An index loaded from disk.
pub struct Loaded {
    pub index: Index,
    pub store: IndexStore,
    /// Sequence number of the last applied change.
    pub seq: u64,
    pub epoch: u64,
    /// The changes replayed from the log (those after the snapshot).
    pub recent: Vec<Logged>,
}

impl IndexStore {
    /// Creates storage for an index, starting from its current contents,
    /// which reflect every change up to `seq`. Replaces anything in `dir`.
    pub fn create(dir: &Path, index: &Index, seq: u64, epoch: u64) -> Result<Self, StorageError> {
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        fs::create_dir_all(dir)?;
        if let Some(parent) = dir.parent() {
            sync_dir(parent)?;
        }
        write_snapshot(dir, 1, index, seq, epoch)?;
        let wal = open_wal(&wal_path(dir, 1))?;
        sync_dir(dir)?;
        Ok(IndexStore { dir: dir.to_owned(), generation: 1, wal, wal_entries: 0, wal_bytes: 0 })
    }

    /// Loads an index from its directory.
    pub fn open(dir: &Path) -> Result<Loaded, StorageError> {
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
        let path = snapshot_path(dir, generation);
        let image = image_path(dir, generation);
        let from_image = if image.exists() {
            read_image(&image).inspect_err(|e| eprintln!("{e}; reading the snapshot instead")).ok()
        } else {
            None
        };
        let has_image = from_image.is_some();
        let SnapshotData { mut index, seq, epoch } = match from_image {
            Some(data) => data,
            None => read_snapshot_from(BufReader::new(File::open(&path)?), &path)?,
        };
        if !has_image && let Some(epoch) = epoch {
            // Rebuild the missing image now (the log is not applied yet, so
            // it matches the snapshot), so the next start is fast.
            if let Err(e) = write_image(dir, generation, &index, seq, epoch) {
                eprintln!("{}: could not write index image: {e}", dir.display());
            }
        }
        let wal = wal_path(dir, generation);
        let replayed =
            if wal.exists() { replay_wal(&wal, &mut index, seq)? } else { Replayed { seq, entries: Vec::new(), bytes: 0 } };
        let file = open_wal(&wal)?;
        // Older generations are fully contained in this one.
        for old in generations.into_iter().filter(|&g| g < generation) {
            let _ = fs::remove_file(snapshot_path(dir, old));
            let _ = fs::remove_file(wal_path(dir, old));
            let _ = fs::remove_file(image_path(dir, old));
        }
        sync_dir(dir)?;
        let mut store = IndexStore {
            dir: dir.to_owned(),
            generation,
            wal: file,
            wal_entries: replayed.entries.len() as u64,
            wal_bytes: replayed.bytes,
        };
        let epoch = match epoch {
            Some(epoch) => epoch,
            None => {
                // A version 1 index: give it an epoch and rewrite it as version 2.
                let epoch = new_epoch();
                store.snapshot(&index, replayed.seq, epoch)?;
                epoch
            }
        };
        Ok(Loaded { index, store, seq: replayed.seq, epoch, recent: replayed.entries })
    }

    /// Appends entries and waits until they are on disk.
    pub fn append(&mut self, entries: &[Logged]) -> Result<(), StorageError> {
        let text: String = entries.iter().map(encode).collect();
        self.wal.write_all(text.as_bytes())?;
        self.wal.sync_data()?;
        self.wal_entries += entries.len() as u64;
        self.wal_bytes += text.len() as u64;
        Ok(())
    }

    /// Whether the log holds changes not in the snapshot.
    pub fn has_log_entries(&self) -> bool {
        self.wal_entries > 0
    }

    pub fn needs_snapshot(&self) -> bool {
        self.wal_entries >= SNAPSHOT_AFTER_ENTRIES || self.wal_bytes >= SNAPSHOT_AFTER_BYTES
    }

    /// Writes `index` (at sequence `seq`) as the next generation and drops
    /// the old log.
    pub fn snapshot(&mut self, index: &Index, seq: u64, epoch: u64) -> Result<(), StorageError> {
        let next = self.generation + 1;
        write_snapshot(&self.dir, next, index, seq, epoch)?;
        let wal = open_wal(&wal_path(&self.dir, next))?;
        sync_dir(&self.dir)?;
        let old = std::mem::replace(&mut self.wal, wal);
        drop(old);
        let _ = fs::remove_file(snapshot_path(&self.dir, self.generation));
        let _ = fs::remove_file(wal_path(&self.dir, self.generation));
        let _ = fs::remove_file(image_path(&self.dir, self.generation));
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
pub fn open_all(root: &Path) -> Result<Vec<(String, Loaded)>, StorageError> {
    let indexes = root.join("indexes");
    fs::create_dir_all(&indexes)?;
    let mut opened = Vec::new();
    for entry in fs::read_dir(&indexes)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        opened.push((name, IndexStore::open(&entry.path())?));
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

    /// Numbers entries after `seq`, writes them to disk and memory, as the
    /// server does. Returns the new sequence number.
    fn write(index: &mut Index, store: &mut IndexStore, seq: u64, entries: Vec<Entry>) -> u64 {
        let logged: Vec<Logged> =
            entries.into_iter().enumerate().map(|(i, entry)| Logged { seq: seq + 1 + i as u64, entry }).collect();
        store.append(&logged).unwrap();
        for l in &logged {
            l.entry.apply(index);
        }
        logged.last().map_or(seq, |l| l.seq)
    }

    fn settings() -> Settings {
        serde_json::from_value(json!({"searchableAttributes": ["name"], "customRanking": ["desc(rating)"]})).unwrap()
    }

    #[test]
    fn changes_survive_reopening_with_their_sequence_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::new(settings());
        let mut store = IndexStore::create(dir.path(), &index, 0, 77).unwrap();
        let seq = write(&mut index, &mut store, 0, vec![
            put(json!({"objectID": "1", "name": "Kettle"})),
            put(json!({"objectID": "2", "name": "Toaster"})),
            Entry::Delete { object_id: "1".into() },
            put(json!({"objectID": "3", "name": "Teapot"})),
        ]);
        assert_eq!(seq, 4);
        drop(store);

        let loaded = IndexStore::open(dir.path()).unwrap();
        assert_eq!(names(&loaded.index), ["Toaster", "Teapot"]);
        assert_eq!(loaded.index.settings().custom_ranking.len(), 1);
        assert_eq!((loaded.seq, loaded.epoch), (4, 77));
        assert_eq!((loaded.store.wal_entries, loaded.store.generation()), (4, 1));
        assert_eq!(loaded.recent.iter().map(|l| l.seq).collect::<Vec<_>>(), [1, 2, 3, 4]);
    }

    #[test]
    fn settings_and_clear_are_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::new(Settings::default());
        let mut store = IndexStore::create(dir.path(), &index, 0, 1).unwrap();
        write(&mut index, &mut store, 0, vec![
            put(json!({"objectID": "1", "name": "Kettle", "brand": "Orbix"})),
            Entry::Clear,
            put(json!({"objectID": "2", "name": "Lamp", "brand": "Orbix"})),
            Entry::Settings { settings: settings() },
        ]);
        drop(store);
        let loaded = IndexStore::open(dir.path()).unwrap();
        assert_eq!(names(&loaded.index), ["Lamp"]);
        // "brand" is no longer searchable after the settings change.
        assert!(loaded.index.word("orbix").is_none());
        assert!(loaded.index.word("lamp").is_some());
    }

    #[test]
    fn torn_last_entry_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::new(Settings::default());
        let mut store = IndexStore::create(dir.path(), &index, 0, 1).unwrap();
        write(&mut index, &mut store, 0, vec![put(json!({"objectID": "1", "name": "Kettle"}))]);
        drop(store);
        // Simulate a crash half-way through writing the next entry.
        let wal = wal_path(dir.path(), 1);
        let good_len = fs::metadata(&wal).unwrap().len();
        let half = &encode(&Logged { seq: 2, entry: put(json!({"objectID": "2", "name": "Lamp"})) })[..20];
        OpenOptions::new().append(true).open(&wal).unwrap().write_all(half.as_bytes()).unwrap();

        let mut loaded = IndexStore::open(dir.path()).unwrap();
        assert_eq!(names(&loaded.index), ["Kettle"]);
        assert_eq!(loaded.seq, 1);
        assert_eq!(fs::metadata(&wal).unwrap().len(), good_len);
        // Writing continues cleanly after the cut.
        write(&mut loaded.index, &mut loaded.store, 1, vec![put(json!({"objectID": "3", "name": "Desk"}))]);
        drop(loaded);
        let loaded = IndexStore::open(dir.path()).unwrap();
        assert_eq!(names(&loaded.index), ["Kettle", "Desk"]);
        assert_eq!(loaded.seq, 2);
    }

    #[test]
    fn damage_before_the_end_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::new(Settings::default());
        let mut store = IndexStore::create(dir.path(), &index, 0, 1).unwrap();
        write(&mut index, &mut store, 0, vec![
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
            Err(e) => panic!("expected corruption, got {e}"),
            Ok(_) => panic!("expected corruption"),
        }
    }

    #[test]
    fn a_gap_in_sequence_numbers_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::new(Settings::default());
        let mut store = IndexStore::create(dir.path(), &index, 0, 1).unwrap();
        write(&mut index, &mut store, 0, vec![put(json!({"objectID": "1"}))]);
        write(&mut index, &mut store, 5, vec![put(json!({"objectID": "2"}))]);
        drop(store);
        match IndexStore::open(dir.path()) {
            Err(StorageError::Corrupt(msg)) => assert!(msg.contains("expected 2"), "{msg}"),
            _ => panic!("expected corruption"),
        }
    }

    #[test]
    fn snapshot_starts_a_new_generation() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::new(settings());
        let mut store = IndexStore::create(dir.path(), &index, 0, 9).unwrap();
        let seq = write(&mut index, &mut store, 0, vec![put(json!({"objectID": "1", "name": "Kettle", "rating": 2}))]);
        store.snapshot(&index, seq, 9).unwrap();
        write(&mut index, &mut store, seq, vec![put(json!({"objectID": "2", "name": "Lamp", "rating": 5}))]);
        assert_eq!(store.generation(), 2);
        let mut files: Vec<String> =
            fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        files.sort();
        assert_eq!(files, ["image-2.bin", "snapshot-2.jsonl", "wal-2.log"]);
        drop(store);
        let loaded = IndexStore::open(dir.path()).unwrap();
        assert_eq!(names(&loaded.index), ["Kettle", "Lamp"]);
        assert_eq!((loaded.seq, loaded.epoch, loaded.store.wal_entries), (2, 9, 1));
    }

    /// An index with a snapshot (generation 2, with its image) and one
    /// change in the log after it.
    fn index_with_image(dir: &Path) {
        let mut index = Index::new(settings());
        let mut store = IndexStore::create(dir, &index, 0, 9).unwrap();
        let seq = write(&mut index, &mut store, 0, vec![
            put(json!({"objectID": "1", "name": "Kettle", "rating": 2})),
            put(json!({"objectID": "2", "name": "Lamp", "rating": 5})),
        ]);
        store.snapshot(&index, seq, 9).unwrap();
        write(&mut index, &mut store, seq, vec![put(json!({"objectID": "3", "name": "Desk", "rating": 1}))]);
    }

    #[test]
    fn startup_reads_the_image_and_replays_the_log_after_it() {
        let dir = tempfile::tempdir().unwrap();
        index_with_image(dir.path());
        // Swap in a different (valid) snapshot: if the image is used, its
        // records are what loads.
        fs::write(
            snapshot_path(dir.path(), 2),
            "{\"version\":2,\"settings\":{},\"seq\":2,\"epoch\":9}\n{\"objectID\":\"9\",\"name\":\"Other\"}\n{\"end\":1}\n",
        )
        .unwrap();
        let loaded = IndexStore::open(dir.path()).unwrap();
        assert_eq!(names(&loaded.index), ["Kettle", "Lamp", "Desk"]);
        assert_eq!((loaded.seq, loaded.epoch), (3, 9));
        assert_eq!(loaded.index.settings().custom_ranking.len(), 1);
        assert!(loaded.index.word("desk").is_some());
    }

    #[test]
    fn a_damaged_or_missing_image_falls_back_to_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        index_with_image(dir.path());
        let image = image_path(dir.path(), 2);
        let mut bytes = fs::read(&image).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        fs::write(&image, &bytes).unwrap();
        let loaded = IndexStore::open(dir.path()).unwrap();
        assert_eq!(names(&loaded.index), ["Kettle", "Lamp", "Desk"]);
        assert_eq!(loaded.seq, 3);
        drop(loaded);

        fs::write(&image, &bytes[..100]).unwrap();
        assert_eq!(names(&IndexStore::open(dir.path()).unwrap().index), ["Kettle", "Lamp", "Desk"]);
        fs::remove_file(&image).unwrap();
        assert_eq!(names(&IndexStore::open(dir.path()).unwrap().index), ["Kettle", "Lamp", "Desk"]);
        // Each fallback wrote a good image again.
        assert!(read_image(&image).is_ok_and(|data| data.seq == 2));
    }

    #[test]
    fn version_1_files_are_upgraded() {
        let dir = tempfile::tempdir().unwrap();
        // Exactly what step 9 wrote: no seq or epoch, bare entries in the log.
        fs::write(
            snapshot_path(dir.path(), 1),
            "{\"version\":1,\"settings\":{}}\n{\"objectID\":\"1\",\"name\":\"Kettle\"}\n{\"end\":1}\n",
        )
        .unwrap();
        let body = r#"{"op":"put","record":{"objectID":"2","name":"Lamp"}}"#;
        fs::write(wal_path(dir.path(), 1), format!("{:08x} {body}\n", crc32fast::hash(body.as_bytes()))).unwrap();

        let loaded = IndexStore::open(dir.path()).unwrap();
        assert_eq!(names(&loaded.index), ["Kettle", "Lamp"]);
        assert_eq!(loaded.seq, 1);
        let epoch = loaded.epoch;
        drop(loaded);
        // The upgrade was written as a version 2 snapshot, keeping the epoch.
        let loaded = IndexStore::open(dir.path()).unwrap();
        assert_eq!((loaded.seq, loaded.epoch, loaded.store.generation()), (1, epoch, 2));
    }

    #[test]
    fn incomplete_snapshot_files_are_ignored_or_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::new(Settings::default());
        drop(IndexStore::create(dir.path(), &index, 0, 1).unwrap());
        // A leftover temporary file from a crash during snapshotting.
        fs::write(dir.path().join("snapshot-2.jsonl.tmp"), "{\"version\":2").unwrap();
        IndexStore::open(dir.path()).unwrap();
        assert!(!dir.path().join("snapshot-2.jsonl.tmp").exists());
        // A snapshot without its trailer is never accepted (without an image
        // to load instead).
        fs::remove_file(image_path(dir.path(), 1)).unwrap();
        fs::write(snapshot_path(dir.path(), 1), "{\"version\":2,\"settings\":{},\"seq\":0,\"epoch\":1}\n").unwrap();
        assert!(matches!(IndexStore::open(dir.path()), Err(StorageError::Corrupt(_))));
    }

    #[test]
    fn snapshots_stream_through_memory() {
        let mut index = Index::new(settings());
        index.add(json!({"objectID": "1", "name": "Kettle"})).unwrap();
        let mut bytes = Vec::new();
        write_snapshot_to(&mut bytes, &index, 42, 5).unwrap();
        let data = read_snapshot_from(&bytes[..], Path::new("memory")).unwrap();
        assert_eq!((data.seq, data.epoch), (42, Some(5)));
        assert_eq!(names(&data.index), ["Kettle"]);
        let cut = &bytes[..bytes.len() - 5];
        assert!(read_snapshot_from(cut, Path::new("memory")).is_err());
    }

    #[test]
    fn open_all_and_destroy() {
        let root = tempfile::tempdir().unwrap();
        for name in ["movies", "products"] {
            let mut index = Index::new(Settings::default());
            index.add(json!({"objectID": "1", "name": name})).unwrap();
            IndexStore::create(&index_dir(root.path(), name), &index, 0, 1).unwrap();
        }
        let opened = open_all(root.path()).unwrap();
        assert_eq!(opened.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["movies", "products"]);
        let (_, loaded) = opened.into_iter().next().unwrap();
        loaded.store.destroy().unwrap();
        assert_eq!(open_all(root.path()).unwrap().len(), 1);
    }
}
