//! Document storage.
//!
//! Users identify records by a string `objectID`. Internally every record gets a
//! dense `DocId` (u32), because posting lists and bitmaps are far smaller and
//! faster with small integers. Ids of deleted records are not reused, so a
//! `DocId` never silently points at a different record.
//!
//! Records are kept as compact JSON text and parsed when read. A parsed
//! JSON tree takes about five times the memory of its text, and searches
//! read only the records of the page they return.

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

/// What `upsert` did.
#[derive(Debug)]
pub struct Upserted {
    pub id: DocId,
    /// The record as stored (objectID normalized to a string).
    pub record: Map<String, Value>,
    /// The record it replaced, if any.
    pub previous: Option<Map<String, Value>>,
}

#[derive(Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct DocumentStore {
    /// Compact JSON per DocId; `None` marks a deleted record.
    docs: Vec<Option<Box<str>>>,
    ids: HashMap<String, DocId>,
}

fn parse(text: &str) -> Map<String, Value> {
    serde_json::from_str(text).expect("stored records are valid JSON objects")
}

impl DocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a record, or replaces the record with the same objectID.
    pub fn upsert(&mut self, record: Value) -> Result<Upserted, StoreError> {
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
        let text: Box<str> = serde_json::to_string(&map).expect("JSON values serialize").into();

        if let Some(&id) = self.ids.get(&object_id) {
            let previous = self.docs[id as usize].replace(text).map(|old| parse(&old));
            return Ok(Upserted { id, record: map, previous });
        }
        let id = DocId::try_from(self.docs.len()).expect("more than u32::MAX records");
        self.docs.push(Some(text));
        self.ids.insert(object_id, id);
        Ok(Upserted { id, record: map, previous: None })
    }

    pub fn delete(&mut self, object_id: &str) -> Option<(DocId, Map<String, Value>)> {
        let id = self.ids.remove(object_id)?;
        let text = self.docs[id as usize].take()?;
        Some((id, parse(&text)))
    }

    /// The record, parsed.
    pub fn get(&self, id: DocId) -> Option<Map<String, Value>> {
        self.get_raw(id).map(parse)
    }

    /// The record as stored: compact JSON.
    pub fn get_raw(&self, id: DocId) -> Option<&str> {
        self.docs.get(id as usize)?.as_deref()
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

    /// Live records, parsed.
    pub fn iter(&self) -> impl Iterator<Item = (DocId, Map<String, Value>)> + '_ {
        self.iter_raw().map(|(id, text)| (id, parse(text)))
    }

    /// Live records as stored (compact JSON), in DocId order.
    pub fn iter_raw(&self) -> impl Iterator<Item = (DocId, &str)> + '_ {
        self.docs.iter().enumerate().filter_map(|(i, d)| d.as_deref().map(|d| (i as DocId, d)))
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
        let first = store.upsert(json!({"objectID": "x", "v": 1})).unwrap();
        assert_eq!((first.id, first.previous), (0, None));
        let second = store.upsert(json!({"objectID": 7, "v": 2})).unwrap();
        assert_eq!(second.id, 1);
        assert_eq!(second.record["objectID"], "7");
        assert_eq!(store.id_of("7"), Some(1));

        let again = store.upsert(json!({"objectID": "x", "v": 3})).unwrap();
        assert_eq!(again.id, 0);
        assert_eq!(again.previous.unwrap()["v"], 1);
        assert_eq!(store.get(0).unwrap()["v"], 3);
        assert_eq!(store.get_raw(0), Some(r#"{"objectID":"x","v":3}"#));
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn deleted_ids_are_not_reused() {
        let mut store = DocumentStore::new();
        store.upsert(json!({"objectID": "a"})).unwrap();
        assert!(store.delete("a").is_some());
        assert!(store.get(0).is_none());
        assert_eq!(store.upsert(json!({"objectID": "a"})).unwrap().id, 1);
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
