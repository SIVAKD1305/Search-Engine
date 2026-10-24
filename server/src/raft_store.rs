//! What a cluster node keeps on disk for Raft, in `DATA_DIR/raft/`:
//!
//! - `hard.json`: the current term and vote, replaced atomically on change.
//! - `log`: the log entries after the snapshot, one record each:
//!   `<length u32><crc32 u32><entry as JSON>`. A torn last record (a crash
//!   mid-write) is cut off; damage elsewhere is an error.
//! - `snapshot`: the applied state up to some entry: a header line
//!   `{"lastIndex":I,"lastTerm":T,"crc":C,"length":L}` and `L` bytes of state
//!   (see `cluster`). Written to a temporary file and renamed into place.
//!
//! Every change is fsynced before Raft's messages go out, as Raft requires.

use crate::raft::{HardState, LogEntry};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotHeader {
    pub last_index: u64,
    pub last_term: u64,
    pub crc: u32,
    pub length: u64,
}

pub struct RaftStore {
    dir: PathBuf,
    log: File,
    /// Index and byte offset of each record in `log`, in order.
    offsets: Vec<(u64, u64)>,
    log_len: u64,
}

/// What `open` found.
pub struct Loaded {
    pub store: RaftStore,
    pub hard: HardState,
    pub snapshot: Option<SnapshotHeader>,
    pub entries: Vec<LogEntry>,
}

fn invalid(path: &Path, message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{}: {message}", path.display()))
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Writes `bytes` to `path` atomically.
fn replace_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    sync_dir(path.parent().expect("files live in a directory"))
}

fn encode(entry: &LogEntry) -> Vec<u8> {
    let body = serde_json::to_vec(entry).expect("entries serialize");
    let mut record = Vec::with_capacity(body.len() + 8);
    record.extend_from_slice(&(body.len() as u32).to_le_bytes());
    record.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
    record.extend_from_slice(&body);
    record
}

impl RaftStore {
    pub fn open(dir: &Path) -> io::Result<Loaded> {
        fs::create_dir_all(dir)?;
        let hard_path = dir.join("hard.json");
        let hard = match fs::read(&hard_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| invalid(&hard_path, e))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => HardState::default(),
            Err(e) => return Err(e),
        };
        let snapshot = Self::read_snapshot_header(&dir.join("snapshot"))?;
        let log_path = dir.join("log");
        let data = match fs::read(&log_path) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let mut entries = Vec::new();
        let mut offsets = Vec::new();
        let mut offset = 0usize;
        while offset < data.len() {
            let rest = &data[offset..];
            let parsed = (rest.len() >= 8)
                .then(|| {
                    let len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
                    let crc = u32::from_le_bytes(rest[4..8].try_into().unwrap());
                    let body = rest.get(8..8 + len)?;
                    (crc32fast::hash(body) == crc).then_some(())?;
                    let entry: LogEntry = serde_json::from_slice(body).ok()?;
                    Some((entry, 8 + len))
                })
                .flatten();
            match parsed {
                Some((entry, size)) => {
                    offsets.push((entry.index, offset as u64));
                    entries.push(entry);
                    offset += size;
                }
                None => {
                    // Only a torn last record is expected; anything after
                    // it would mean damage in the middle.
                    let torn_tail = data.len() - offset < 8 || {
                        let len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
                        offset + 8 + len >= data.len()
                    };
                    if !torn_tail {
                        return Err(invalid(&log_path, format!("damaged record at byte {offset}")));
                    }
                    eprintln!("{}: dropping an incomplete last record", log_path.display());
                    OpenOptions::new().write(true).open(&log_path)?.set_len(offset as u64)?;
                    break;
                }
            }
        }
        // Entries the snapshot holds may remain after a crash during compaction.
        let first_needed = snapshot.map_or(1, |s| s.last_index + 1);
        if entries.iter().any(|e| e.index < first_needed) || entries.windows(2).any(|w| w[1].index != w[0].index + 1) {
            let keep: Vec<LogEntry> = entries.into_iter().filter(|e| e.index >= first_needed).collect();
            if keep.windows(2).any(|w| w[1].index != w[0].index + 1) || keep.first().is_some_and(|e| e.index != first_needed) {
                return Err(invalid(&log_path, "entries are not contiguous"));
            }
            let mut store = RaftStore { dir: dir.to_owned(), log: Self::open_log(&log_path)?, offsets: Vec::new(), log_len: 0 };
            store.rewrite(&keep)?;
            return Ok(Loaded { store, hard, snapshot, entries: keep });
        }
        if entries.first().is_some_and(|e| e.index != first_needed) {
            return Err(invalid(&log_path, "a gap after the snapshot"));
        }
        let log = Self::open_log(&log_path)?;
        let store = RaftStore { dir: dir.to_owned(), log, offsets, log_len: offset as u64 };
        Ok(Loaded { store, hard, snapshot, entries })
    }

    fn open_log(path: &Path) -> io::Result<File> {
        OpenOptions::new().create(true).read(true).append(true).open(path)
    }

    pub fn save_hard_state(&self, hard: &HardState) -> io::Result<()> {
        replace_file(&self.dir.join("hard.json"), &serde_json::to_vec(hard)?)
    }

    /// Cuts the log at `from` (inclusive), appends `entries`, and syncs.
    pub fn write_log(&mut self, from: u64, entries: &[LogEntry]) -> io::Result<()> {
        let cut = self.offsets.partition_point(|&(index, _)| index < from);
        if cut < self.offsets.len() {
            self.log_len = self.offsets[cut].1;
            self.offsets.truncate(cut);
            self.log.set_len(self.log_len)?;
        }
        let mut buffer = Vec::new();
        for entry in entries {
            self.offsets.push((entry.index, self.log_len + buffer.len() as u64));
            buffer.extend_from_slice(&encode(entry));
        }
        self.log.write_all(&buffer)?;
        self.log.sync_data()?;
        self.log_len += buffer.len() as u64;
        Ok(())
    }

    /// Replaces the log with `entries` (after compaction).
    pub fn rewrite(&mut self, entries: &[LogEntry]) -> io::Result<()> {
        let path = self.dir.join("log");
        let tmp = self.dir.join("log.tmp");
        let mut buffer = Vec::new();
        let mut offsets = Vec::with_capacity(entries.len());
        for entry in entries {
            offsets.push((entry.index, buffer.len() as u64));
            buffer.extend_from_slice(&encode(entry));
        }
        let mut file = File::create(&tmp)?;
        file.write_all(&buffer)?;
        file.sync_all()?;
        fs::rename(&tmp, &path)?;
        sync_dir(&self.dir)?;
        self.log = Self::open_log(&path)?;
        self.offsets = offsets;
        self.log_len = buffer.len() as u64;
        Ok(())
    }

    /// Bytes in the log file.
    pub fn log_bytes(&self) -> u64 {
        self.log_len
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.dir.join("snapshot")
    }

    /// Where a snapshot received from the leader is written first.
    pub fn incoming_path(&self) -> PathBuf {
        self.dir.join("snapshot.incoming")
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The last index of the snapshot at `path`, if there is one.
    pub fn snapshot_index(path: &Path) -> io::Result<Option<u64>> {
        Ok(Self::read_snapshot_header(path)?.map(|h| h.last_index))
    }

    fn read_snapshot_header(path: &Path) -> io::Result<Option<SnapshotHeader>> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut line = Vec::new();
        for byte in BufReader::new(file).bytes() {
            let byte = byte?;
            if byte == b'\n' {
                break;
            }
            line.push(byte);
        }
        serde_json::from_slice(&line).map(Some).map_err(|e| invalid(path, e))
    }

    /// Writes a snapshot file at `path` (via a temporary file), returning
    /// its header.
    pub fn write_snapshot(path: &Path, last_index: u64, last_term: u64, state: &[u8]) -> io::Result<SnapshotHeader> {
        let header =
            SnapshotHeader { last_index, last_term, crc: crc32fast::hash(state), length: state.len() as u64 };
        let mut bytes = serde_json::to_vec(&header)?;
        bytes.push(b'\n');
        bytes.extend_from_slice(state);
        replace_file(path, &bytes)?;
        Ok(header)
    }

    /// Reads and checks a snapshot file: its header and state bytes.
    pub fn read_snapshot(path: &Path) -> io::Result<(SnapshotHeader, Vec<u8>)> {
        let bytes = fs::read(path)?;
        Self::parse_snapshot(&bytes).map_err(|e| invalid(path, e))
    }

    /// Splits snapshot file bytes into header and state, checking them.
    pub fn parse_snapshot(bytes: &[u8]) -> Result<(SnapshotHeader, Vec<u8>), String> {
        let newline = bytes.iter().position(|&b| b == b'\n').ok_or("no snapshot header")?;
        let header: SnapshotHeader = serde_json::from_slice(&bytes[..newline]).map_err(|e| e.to_string())?;
        let state = &bytes[newline + 1..];
        if state.len() as u64 != header.length || crc32fast::hash(state) != header.crc {
            return Err("snapshot is incomplete or damaged".into());
        }
        Ok((header, state.to_vec()))
    }

    /// Makes the received snapshot the current one.
    pub fn accept_incoming(&self) -> io::Result<()> {
        fs::rename(self.incoming_path(), self.snapshot_path())?;
        sync_dir(&self.dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(index: u64, term: u64) -> LogEntry {
        LogEntry { index, term, command: format!("c{index}") }
    }

    #[test]
    fn log_survives_reopening_with_truncation_and_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = RaftStore::open(dir.path()).unwrap();
        assert!(loaded.entries.is_empty() && loaded.snapshot.is_none());
        loaded.store.save_hard_state(&HardState { term: 3, voted_for: Some("b".into()) }).unwrap();
        loaded.store.write_log(1, &[entry(1, 1), entry(2, 1), entry(3, 2)]).unwrap();
        // A conflicting suffix replaced from index 3.
        loaded.store.write_log(3, &[entry(3, 3), entry(4, 3)]).unwrap();
        drop(loaded);

        let mut loaded = RaftStore::open(dir.path()).unwrap();
        assert_eq!(loaded.hard, HardState { term: 3, voted_for: Some("b".into()) });
        assert_eq!(loaded.entries, [entry(1, 1), entry(2, 1), entry(3, 3), entry(4, 3)]);
        // Compaction: a snapshot at 2, then the log rewritten.
        let path = loaded.store.snapshot_path();
        RaftStore::write_snapshot(&path, 2, 1, b"state").unwrap();
        loaded.store.rewrite(&[entry(3, 3), entry(4, 3)]).unwrap();
        loaded.store.write_log(5, &[entry(5, 3)]).unwrap();
        drop(loaded);

        let loaded = RaftStore::open(dir.path()).unwrap();
        assert_eq!(loaded.snapshot.map(|s| (s.last_index, s.last_term)), Some((2, 1)));
        assert_eq!(loaded.entries, [entry(3, 3), entry(4, 3), entry(5, 3)]);
        let (_, state) = RaftStore::read_snapshot(&path).unwrap();
        assert_eq!(state, b"state");
    }

    #[test]
    fn a_torn_last_record_is_dropped_and_damage_elsewhere_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = RaftStore::open(dir.path()).unwrap();
        loaded.store.write_log(1, &[entry(1, 1), entry(2, 1)]).unwrap();
        drop(loaded);
        let log = dir.path().join("log");
        let good = fs::read(&log).unwrap();
        let mut torn = good.clone();
        torn.extend_from_slice(&encode(&entry(3, 1))[..10]);
        fs::write(&log, &torn).unwrap();
        let loaded = RaftStore::open(dir.path()).unwrap();
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(fs::read(&log).unwrap(), good);
        drop(loaded);

        let mut damaged = good.clone();
        damaged[10] ^= 0xff; // inside the first record
        fs::write(&log, &damaged).unwrap();
        assert!(RaftStore::open(dir.path()).is_err());
    }

    #[test]
    fn entries_covered_by_the_snapshot_are_dropped_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = RaftStore::open(dir.path()).unwrap();
        loaded.store.write_log(1, &[entry(1, 1), entry(2, 1), entry(3, 1)]).unwrap();
        // A crash after the snapshot was written but before the log rewrite.
        RaftStore::write_snapshot(&loaded.store.snapshot_path(), 2, 1, b"s").unwrap();
        drop(loaded);
        let loaded = RaftStore::open(dir.path()).unwrap();
        assert_eq!(loaded.entries, [entry(3, 1)]);
    }

    #[test]
    fn damaged_snapshots_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot");
        RaftStore::write_snapshot(&path, 7, 2, b"some state").unwrap();
        let mut bytes = fs::read(&path).unwrap();
        assert!(RaftStore::parse_snapshot(&bytes).is_ok());
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(RaftStore::parse_snapshot(&bytes).is_err());
        assert!(RaftStore::parse_snapshot(&bytes[..bytes.len() - 3]).is_err());
    }
}
