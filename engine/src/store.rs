//! Document storage.
//!
//! Users identify records by a string `objectID`. Internally every record gets a
//! dense `DocId` (u32), because posting lists and bitmaps are far smaller and
//! faster with small integers. Ids of deleted records are not reused, so a
//! `DocId` never silently points at a different record.

use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt;
use std::io::BufRead;

pub type DocId = u32;

#[derive(Debug)]
pub enum StoreError {
    NotAnObject,
    MissingObjectId,
    Json { line: usize, source: serde_json::Error },
    Io(std::io::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotAnObject => write!(f, "record must be a JSON object"),
            StoreError::MissingObjectId => write!(f, "record needs a string or number objectID"),
            StoreError::Json { line, source } => write!(f, "invalid JSON on line {line}: {source}"),
            StoreError::Io(e) => write!(f, "read failed: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

#[derive(Default)]
pub struct DocumentStore {
    /// Indexed by DocId; `None` marks a deleted record.
    docs: Vec<Option<Map<String, Value>>>,
    ids: HashMap<String, DocId>,
}

impl DocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a record, or replaces the record with the same objectID.
    /// Returns its DocId and the previous version, if any.
    pub fn upsert(&mut self, record: Value) -> Result<(DocId, Option<Map<String, Value>>), StoreError> {
        let Value::Object(mut map) = record else {
            return Err(StoreError::NotAnObject);
        };
        let object_id = match map.get("objectID") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => return Err(StoreError::MissingObjectId),
        };
        // Store the objectID normalized as a string, as Algolia returns it.
        map.insert("objectID".into(), Value::String(object_id.clone()));

        if let Some(&id) = self.ids.get(&object_id) {
            let previous = self.docs[id as usize].replace(map);
            return Ok((id, previous));
        }
        let id = DocId::try_from(self.docs.len()).expect("more than u32::MAX records");
        self.docs.push(Some(map));
        self.ids.insert(object_id, id);
        Ok((id, None))
    }

    pub fn delete(&mut self, object_id: &str) -> Option<(DocId, Map<String, Value>)> {
        let id = self.ids.remove(object_id)?;
        let doc = self.docs[id as usize].take()?;
        Some((id, doc))
    }

    pub fn get(&self, id: DocId) -> Option<&Map<String, Value>> {
        self.docs.get(id as usize)?.as_ref()
    }

    pub fn id_of(&self, object_id: &str) -> Option<DocId> {
        self.ids.get(object_id).copied()
    }

    /// Number of live records.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (DocId, &Map<String, Value>)> {
        self.docs
            .iter()
            .enumerate()
            .filter_map(|(i, d)| d.as_ref().map(|d| (i as DocId, d)))
    }

    /// Loads JSON Lines (one record per line). Blank lines are skipped.
    pub fn load_jsonl(&mut self, reader: impl BufRead) -> Result<usize, StoreError> {
        let mut loaded = 0;
        for (i, line) in reader.lines().enumerate() {
            let line = line.map_err(StoreError::Io)?;
            if line.trim().is_empty() {
                continue;
            }
            let value = serde_json::from_str(&line)
                .map_err(|source| StoreError::Json { line: i + 1, source })?;
            self.upsert(value)?;
            loaded += 1;
        }
        Ok(loaded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn upsert_assigns_dense_ids_and_replaces_by_object_id() {
        let mut store = DocumentStore::new();
        let (a, prev) = store.upsert(json!({"objectID": "x", "v": 1})).unwrap();
        assert_eq!((a, prev), (0, None));
        let (b, _) = store.upsert(json!({"objectID": 7, "v": 2})).unwrap();
        assert_eq!(b, 1);
        assert_eq!(store.id_of("7"), Some(1));

        let (again, prev) = store.upsert(json!({"objectID": "x", "v": 3})).unwrap();
        assert_eq!(again, 0);
        assert_eq!(prev.unwrap()["v"], 1);
        assert_eq!(store.get(0).unwrap()["v"], 3);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn deleted_ids_are_not_reused() {
        let mut store = DocumentStore::new();
        store.upsert(json!({"objectID": "a"})).unwrap();
        assert!(store.delete("a").is_some());
        assert!(store.get(0).is_none());
        let (id, _) = store.upsert(json!({"objectID": "a"})).unwrap();
        assert_eq!(id, 1);
        assert_eq!(store.iter().map(|(id, _)| id).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn rejects_bad_records() {
        let mut store = DocumentStore::new();
        assert!(matches!(store.upsert(json!([1])), Err(StoreError::NotAnObject)));
        assert!(matches!(store.upsert(json!({"name": "x"})), Err(StoreError::MissingObjectId)));
        assert!(matches!(store.upsert(json!({"objectID": ""})), Err(StoreError::MissingObjectId)));
    }

    #[test]
    fn load_jsonl_reports_line_numbers() {
        let mut store = DocumentStore::new();
        let input = "{\"objectID\":\"1\"}\n\n{bad json}\n";
        match store.load_jsonl(input.as_bytes()) {
            Err(StoreError::Json { line, .. }) => assert_eq!(line, 3),
            other => panic!("expected JSON error, got {other:?}"),
        }
    }
}
