//! The inverted index.
//!
//! Two structures, following Algolia's design:
//!
//! - `words`: a sorted dictionary from each full word to its postings — which
//!   records contain it, and in which attribute and position. Ranking reads
//!   these hits.
//! - `prefixes`: for every prefix of every word ("h", "he", "hea", ...) the set
//!   of records containing a word that starts with it. This costs memory but
//!   makes search-as-you-type a single lookup instead of a union over every
//!   matching word. Only record ids are kept here, as compressed bitmaps.

use crate::store::{DocId, DocumentStore, StoreError};
use crate::tokenizer::tokenize_value;
use crate::facets::{FacetAttribute, FacetCount, FacetIndex, NumericIndex};
use crate::ranking::{CustomRule, Ranking};
use crate::typo::TypoSettings;
use serde::{Deserialize, Serialize};
use roaring::RoaringBitmap;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;

/// Index of an attribute in `searchable_attributes`; lower ranks higher.
pub type AttributeId = u16;

/// One occurrence of a word in a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub attribute: AttributeId,
    pub position: u32,
}

#[derive(Debug, Default)]
pub struct Postings {
    pub docs: RoaringBitmap,
    /// Hits per record, sorted by (attribute, position).
    pub hits: HashMap<DocId, Vec<Hit>>,
}

/// Index settings, read from JSON with Algolia's names, e.g.
/// `{"searchableAttributes": ["name"], "customRanking": ["desc(rating)"]}`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Attributes to index, most important first. `None` indexes every
    /// top-level attribute in the order first seen.
    pub searchable_attributes: Option<Vec<String>>,
    #[serde(flatten)]
    pub typo: TypoSettings,
    /// Order of the tie-breaking criteria.
    pub ranking: Ranking,
    /// Numeric attributes used by the `custom` criterion, most important first.
    pub custom_ranking: Vec<CustomRule>,
    /// Attributes usable in facet filters and facet counts.
    pub attributes_for_faceting: Vec<FacetAttribute>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStats {
    pub records: usize,
    pub words: usize,
    pub prefixes: usize,
    pub hits: usize,
}

#[derive(Default)]
pub struct Index {
    store: DocumentStore,
    settings: Settings,
    attributes: Vec<String>,
    words: BTreeMap<String, Postings>,
    prefixes: HashMap<String, RoaringBitmap>,
    facets: FacetIndex,
    numbers: NumericIndex,
    /// Ids of all live records.
    live: RoaringBitmap,
}

impl Index {
    pub fn new(settings: Settings) -> Self {
        let attributes = settings.searchable_attributes.clone().unwrap_or_default();
        let facets = FacetIndex::new(&settings.attributes_for_faceting);
        Index { settings, attributes, facets, ..Default::default() }
    }

    pub fn store(&self) -> &DocumentStore {
        &self.store
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The sorted word dictionary, for typo-tolerant lookups.
    pub fn dictionary(&self) -> &BTreeMap<String, Postings> {
        &self.words
    }

    pub fn attributes(&self) -> &[String] {
        &self.attributes
    }

    /// Adds or replaces a record (matched by objectID).
    pub fn add(&mut self, record: Value) -> Result<DocId, StoreError> {
        let (id, previous) = self.store.upsert(record)?;
        if let Some(old) = previous {
            self.unindex(id, &old);
        }
        let doc = self.store.get(id).expect("just inserted").clone();
        self.index(id, &doc);
        Ok(id)
    }

    /// Removes a record. Returns false if no record has that objectID.
    pub fn delete(&mut self, object_id: &str) -> bool {
        match self.store.delete(object_id) {
            Some((id, old)) => {
                self.unindex(id, &old);
                true
            }
            None => false,
        }
    }

    pub fn load_jsonl(&mut self, reader: impl std::io::BufRead) -> Result<usize, StoreError> {
        let mut loaded = 0;
        for (i, line) in reader.lines().enumerate() {
            let line = line.map_err(StoreError::Io)?;
            if line.trim().is_empty() {
                continue;
            }
            let value = serde_json::from_str(&line)
                .map_err(|source| StoreError::Json { line: i + 1, source })?;
            self.add(value)?;
            loaded += 1;
        }
        Ok(loaded)
    }

    /// Records containing exactly this (normalized) word.
    pub fn word(&self, word: &str) -> Option<&Postings> {
        self.words.get(word)
    }

    /// Records containing a word that starts with this (normalized) prefix.
    pub fn prefix_docs(&self, prefix: &str) -> Option<&RoaringBitmap> {
        self.prefixes.get(prefix)
    }

    /// Dictionary words starting with `prefix`, in sorted order.
    pub fn words_with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.words
            .range::<str, _>((Bound::Included(prefix), Bound::Unbounded))
            .map(|(w, _)| w.as_str())
            .take_while(move |w| w.starts_with(prefix))
    }

    /// Ids of all live records.
    pub fn all_docs(&self) -> RoaringBitmap {
        self.live.clone()
    }

    pub fn is_facet(&self, attribute: &str) -> bool {
        self.facets.contains(attribute)
    }

    /// Facet attributes that return counts (not `filterOnly`), in settings order.
    pub fn countable_facets(&self) -> impl Iterator<Item = &str> {
        self.settings.attributes_for_faceting.iter().filter(|f| !f.filter_only).map(|f| f.attribute.as_str())
    }

    /// Records whose facet `attribute` has `value` (case- and accent-insensitive).
    pub fn facet_docs(&self, attribute: &str, value: &str) -> RoaringBitmap {
        self.facets.docs(attribute, value)
    }

    pub fn facet_counts(&self, attribute: &str, candidates: &RoaringBitmap, limit: usize) -> Vec<FacetCount> {
        self.facets.counts(attribute, candidates, limit)
    }

    /// Records with a numeric `attribute` value inside the bounds.
    pub fn numeric_docs(&self, attribute: &str, low: Bound<f64>, high: Bound<f64>) -> RoaringBitmap {
        self.numbers.docs(attribute, low, high)
    }

    pub fn stats(&self) -> IndexStats {
        IndexStats {
            records: self.store.len(),
            words: self.words.len(),
            prefixes: self.prefixes.len(),
            hits: self.words.values().flat_map(|p| p.hits.values()).map(Vec::len).sum(),
        }
    }

    /// Attribute ids for a record's searchable fields, registering new
    /// attribute names when no explicit list is configured.
    fn searchable_fields<'d>(&mut self, doc: &'d Map<String, Value>) -> Vec<(AttributeId, &'d Value)> {
        let mut fields = Vec::new();
        if self.settings.searchable_attributes.is_some() {
            for (i, name) in self.attributes.iter().enumerate() {
                if let Some(value) = doc.get(name) {
                    fields.push((i as AttributeId, value));
                }
            }
        } else {
            for (name, value) in doc {
                if name == "objectID" {
                    continue;
                }
                let id = match self.attributes.iter().position(|a| a == name) {
                    Some(i) => i,
                    None => {
                        self.attributes.push(name.clone());
                        self.attributes.len() - 1
                    }
                };
                let id = AttributeId::try_from(id).expect("too many attributes");
                fields.push((id, value));
            }
        }
        fields
    }

    fn index(&mut self, id: DocId, doc: &Map<String, Value>) {
        self.live.insert(id);
        self.facets.add(id, doc);
        self.numbers.add(id, doc);
        let mut hits: HashMap<String, Vec<Hit>> = HashMap::new();
        for (attribute, value) in self.searchable_fields(doc) {
            for token in tokenize_value(value) {
                hits.entry(token.text).or_default().push(Hit { attribute, position: token.position });
            }
        }

        let prefixes: HashSet<&str> = hits.keys().flat_map(|w| prefixes_of(w)).collect();
        for prefix in prefixes {
            match self.prefixes.get_mut(prefix) {
                Some(docs) => {
                    docs.insert(id);
                }
                None => {
                    self.prefixes.insert(prefix.to_owned(), RoaringBitmap::from_iter([id]));
                }
            }
        }
        for (word, mut word_hits) in hits {
            word_hits.sort_unstable_by_key(|h| (h.attribute, h.position));
            let postings = self.words.entry(word).or_default();
            postings.docs.insert(id);
            postings.hits.insert(id, word_hits);
        }
    }

    /// Removes a record's postings. The old record is re-tokenized to find its
    /// words, so no per-record word list has to be kept in memory.
    fn unindex(&mut self, id: DocId, old: &Map<String, Value>) {
        self.live.remove(id);
        self.facets.remove(id, old);
        self.numbers.remove(id, old);
        let mut words = HashSet::new();
        for (_, value) in self.searchable_fields(old) {
            words.extend(tokenize_value(value).into_iter().map(|t| t.text));
        }
        // Removing the record from every prefix of every word it had is safe:
        // the whole record is going away, so no other word of it still needs them.
        let mut prefixes = HashSet::new();
        for word in &words {
            prefixes.extend(prefixes_of(word));
            if let Some(postings) = self.words.get_mut(word) {
                postings.docs.remove(id);
                postings.hits.remove(&id);
                if postings.docs.is_empty() {
                    self.words.remove(word);
                }
            }
        }
        for prefix in prefixes {
            if let Some(docs) = self.prefixes.get_mut(prefix) {
                docs.remove(id);
                if docs.is_empty() {
                    self.prefixes.remove(prefix);
                }
            }
        }
    }
}

/// Every non-empty prefix of `word`, cut on character boundaries, including
/// the word itself.
fn prefixes_of(word: &str) -> impl Iterator<Item = &str> {
    word.char_indices().skip(1).map(|(i, _)| &word[..i]).chain(std::iter::once(word))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Index {
        let mut index = Index::new(Settings {
            searchable_attributes: Some(vec!["name".into(), "description".into()]),
            ..Settings::default()
        });
        index.add(json!({"objectID": "1", "name": "Wireless Headphones", "description": "Long battery life", "price": 99})).unwrap();
        index.add(json!({"objectID": "2", "name": "Headset Stand", "description": "Holds wireless headsets"})).unwrap();
        index.add(json!({"objectID": "3", "name": "Kettle", "description": "Boils water fast"})).unwrap();
        index
    }

    fn ids(bitmap: &RoaringBitmap) -> Vec<u32> {
        bitmap.iter().collect()
    }

    #[test]
    fn prefixes_respect_character_boundaries() {
        assert_eq!(prefixes_of("élan").collect::<Vec<_>>(), ["é", "él", "éla", "élan"]);
        assert_eq!(prefixes_of("a").collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn word_postings_record_attribute_and_position() {
        let index = sample();
        let wireless = index.word("wireless").unwrap();
        assert_eq!(ids(&wireless.docs), [0, 1]);
        assert_eq!(wireless.hits[&0], [Hit { attribute: 0, position: 0 }]);
        assert_eq!(wireless.hits[&1], [Hit { attribute: 1, position: 1 }]);
        // Only searchable attributes are indexed.
        assert!(index.word("99").is_none());
    }

    #[test]
    fn prefix_lookup_covers_all_words_with_that_start() {
        let index = sample();
        assert_eq!(ids(index.prefix_docs("head").unwrap()), [0, 1]);
        assert_eq!(ids(index.prefix_docs("heads").unwrap()), [1]);
        assert_eq!(ids(index.prefix_docs("headphones").unwrap()), [0]);
        assert!(index.prefix_docs("headx").is_none());
        let words: Vec<&str> = index.words_with_prefix("head").collect();
        assert_eq!(words, ["headphones", "headset", "headsets"]);
    }

    #[test]
    fn replacing_a_record_removes_its_old_words() {
        let mut index = sample();
        index.add(json!({"objectID": "3", "name": "Teapot"})).unwrap();
        assert!(index.word("kettle").is_none());
        assert!(index.prefix_docs("ket").is_none());
        // "boils" was only in record 3's old description.
        assert!(index.word("boils").is_none());
        assert_eq!(ids(&index.word("teapot").unwrap().docs), [2]);
    }

    #[test]
    fn delete_keeps_prefixes_shared_with_other_records() {
        let mut index = sample();
        assert!(index.delete("2"));
        assert!(!index.delete("2"));
        assert_eq!(ids(index.prefix_docs("head").unwrap()), [0]);
        assert!(index.prefix_docs("heads").is_none());
        assert_eq!(ids(&index.word("wireless").unwrap().docs), [0]);
        assert_eq!(ids(&index.all_docs()), [0, 2]);
    }

    #[test]
    fn default_settings_index_every_attribute_in_first_seen_order() {
        let mut index = Index::new(Settings::default());
        index.add(json!({"objectID": "1", "title": "Iron Harbor", "year": 1996})).unwrap();
        assert_eq!(index.attributes(), ["title", "year"]);
        assert_eq!(ids(&index.word("1996").unwrap().docs), [0]);
    }

    #[test]
    fn repeated_words_keep_every_position() {
        let mut index = Index::new(Settings::default());
        index.add(json!({"objectID": "1", "body": "fast and fast again"})).unwrap();
        let hits = &index.word("fast").unwrap().hits[&0];
        assert_eq!(hits.iter().map(|h| h.position).collect::<Vec<_>>(), [0, 2]);
        assert_eq!(index.stats().hits, 4);
    }
}
