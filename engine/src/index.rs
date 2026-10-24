//! The inverted index.
//!
//! Two structures, following Algolia's design:
//!
//! - `words`: a sorted dictionary from each full word to its postings: the
//!   records containing it, overall and per attribute.
//! - `occurrences`: for each record, every word it contains with its
//!   attribute and position, in reading order. Proximity and highlighting
//!   ranks read these; a record has a few dozen, so looking up the words of a
//!   prefix that matches thousands of words stays cheap.
//! - `prefixes`: for every prefix of every word ("h", "he", "hea", ...) the set
//!   of records containing a word that starts with it. This costs memory but
//!   makes search-as-you-type a single lookup instead of a union over every
//!   matching word. Only record ids are kept here, as compressed bitmaps.

use crate::store::{DocId, DocumentStore, StoreError};
use crate::tokenizer::{HanLanguage, Segmentation, tokenize_value};
use crate::facets::{FacetAttribute, FacetCount, FacetIndex, FacetStats, NumericIndex};
use crate::geo::GeoIndex;
use crate::ranking::{CustomRule, Ranking};
use crate::search::RemoveWords;
use crate::synonyms::{self, Synonym};
use crate::typo::TypoSettings;
use serde::{Deserialize, Serialize};
use roaring::RoaringBitmap;
use serde_json::{Map, Value};
use rustc_hash::{FxHashMap, FxHashSet};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

/// Index of an attribute in `searchable_attributes`; lower ranks higher.
pub type AttributeId = u16;

/// One occurrence of a word in a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub attribute: AttributeId,
    pub position: u32,
}

/// Prefixes up to this many characters have precomputed record sets. Longer
/// prefixes are shared by few words, so they are computed when queried; this
/// keeps indexing fast and memory low (Algolia makes the same trade-off).
pub const MAX_STORED_PREFIX: usize = 4;

/// A dictionary word's permanent number; ids of removed words are not reused.
pub type WordId = u32;

#[derive(Debug, Deserialize, Serialize)]
pub struct Postings {
    pub id: WordId,
    pub word: String,
    pub docs: RoaringBitmap,
    /// Records containing the word in each attribute (indexed by AttributeId).
    pub by_attribute: Vec<RoaringBitmap>,
}

/// One word of a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct Occurrence {
    pub word: WordId,
    pub attribute: AttributeId,
    pub position: u32,
}

impl Occurrence {
    pub fn hit(&self) -> Hit {
        Hit { attribute: self.attribute, position: self.position }
    }
}

/// Records containing a word that starts with a given prefix.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct PrefixDocs {
    pub docs: RoaringBitmap,
    /// The same, per attribute (indexed by AttributeId).
    pub by_attribute: Vec<RoaringBitmap>,
}

/// A small set of attribute ids: a bit mask for the first 64, a list for
/// the rare rest. Allocates nothing in the common case.
#[derive(Default)]
struct AttributeSet {
    mask: u64,
    more: Vec<AttributeId>,
}

impl AttributeSet {
    fn insert(&mut self, a: AttributeId) {
        if a < 64 {
            self.mask |= 1 << a;
        } else if !self.more.contains(&a) {
            self.more.push(a);
        }
    }

    fn iter(&self) -> impl Iterator<Item = AttributeId> + '_ {
        (0..64u16).filter(|a| self.mask & (1 << a) != 0).chain(self.more.iter().copied())
    }
}

fn insert_in_attribute(sets: &mut Vec<RoaringBitmap>, attribute: AttributeId, id: DocId) {
    let a = usize::from(attribute);
    if sets.len() <= a {
        sets.resize_with(a + 1, RoaringBitmap::new);
    }
    sets[a].insert(id);
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
    /// Treat English singular and plural forms as the same word.
    pub ignore_plurals: bool,
    /// Drop common English words ("the", "of", ...) from queries.
    pub remove_stop_words: bool,
    /// What to do when no record matches every query word.
    pub remove_words_if_no_results: RemoveWords,
    /// Query words that are not required to match.
    pub optional_words: Vec<String>,
    /// Algolia keeps synonyms in a separate API; here they are a setting.
    pub synonyms: Vec<Synonym>,
    /// Languages of the records. With "ja" (and not "zh"), text written
    /// only in kanji is split into words as Japanese instead of Chinese.
    pub index_languages: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStats {
    pub records: usize,
    pub words: usize,
    pub prefixes: usize,
    pub hits: usize,
}

/// `Serialize` writes an index image (see `write_image`); the skipped
/// fields are rebuilt when it is read.
#[derive(Default, Deserialize, Serialize)]
pub struct Index {
    store: DocumentStore,
    #[serde(skip)]
    settings: Settings,
    attributes: Vec<String>,
    /// The dictionary in sorted order, for typo walks and prefix ranges.
    words: BTreeMap<String, WordId>,
    /// The same, for fast exact lookups.
    #[serde(skip)]
    word_ids: FxHashMap<String, WordId>,
    /// Indexed by WordId; `None` for removed words.
    postings: Vec<Option<Postings>>,
    prefixes: FxHashMap<String, PrefixDocs>,
    facets: FacetIndex,
    numbers: NumericIndex,
    /// Records' `_geoloc` points.
    geo: GeoIndex,
    /// Ids of all live records.
    live: RoaringBitmap,
    /// Phrases and what they can stand for, from `settings.synonyms`.
    #[serde(skip)]
    synonym_map: synonyms::SynonymMap,
    /// One column per `customRanking` rule: the rule's value for each DocId
    /// (NaN when missing), so ranking never reads record JSON.
    custom_columns: Vec<Vec<f64>>,
    /// Each record's words, sorted by (attribute, position); indexed by DocId.
    occurrences: Vec<Vec<Occurrence>>,
    next_word_id: WordId,
    /// Every live record in `customRanking` order, kept up to date on each
    /// write (only with 1 to MAX_ORDERED_RULES rules).
    custom_order: BTreeSet<(CustomKey, DocId)>,
}

/// Changes whenever the layout of `Index` (or anything inside it) changes,
/// so an old image is rejected instead of misread.
const IMAGE_VERSION: u32 = 4;

/// Records with up to this many `customRanking` rules keep a sorted order.
pub const MAX_ORDERED_RULES: usize = 4;

/// A record's `customRanking` values as one sortable key: smaller sorts
/// first, directions are applied, missing values sort last.
pub type CustomKey = [u64; MAX_ORDERED_RULES];

/// Maps an f64 to a u64 with the same order (below u64::MAX - 1).
pub(crate) fn ordered(v: f64) -> u64 {
    let bits = (v + 0.0).to_bits();
    let key = if bits >> 63 == 1 { !bits } else { bits | (1 << 63) };
    key.min(u64::MAX - 2)
}

impl Index {
    pub fn new(settings: Settings) -> Self {
        let attributes = settings.searchable_attributes.clone().unwrap_or_default();
        let facets = FacetIndex::new(&settings.attributes_for_faceting);
        let synonym_map = synonyms::build_map(&settings.synonyms, Segmentation { han: HanLanguage::from_languages(&settings.index_languages), parts: false });
        let custom_columns = vec![Vec::new(); settings.custom_ranking.len()];
        Index { settings, attributes, facets, synonym_map, custom_columns, ..Default::default() }
    }

    pub fn store(&self) -> &DocumentStore {
        &self.store
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// How this index splits Chinese and Japanese text into words; the same
    /// for records and queries.
    pub fn segmentation(&self) -> Segmentation {
        Segmentation { han: HanLanguage::from_languages(&self.settings.index_languages), parts: false }
    }

    /// Phrases that `words` (normalized) can stand for.
    pub fn synonyms_of(&self, words: &[String]) -> &[Vec<String>] {
        self.synonym_map.get(words)
    }

    /// The longest phrase that has synonyms, in words.
    pub fn longest_synonym(&self) -> usize {
        self.synonym_map.longest()
    }

    /// The sorted word dictionary, for typo-tolerant lookups.
    pub fn dictionary(&self) -> &BTreeMap<String, WordId> {
        &self.words
    }

    pub fn attributes(&self) -> &[String] {
        &self.attributes
    }

    /// Adds or replaces a record (matched by objectID).
    pub fn add(&mut self, record: Value) -> Result<DocId, StoreError> {
        let upserted = self.store.upsert(record)?;
        if let Some(old) = &upserted.previous {
            self.unindex(upserted.id, old);
        }
        self.index(upserted.id, &upserted.record);
        Ok(upserted.id)
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

    /// Replaces the settings and re-indexes every record, keeping their order.
    pub fn set_settings(&mut self, settings: Settings) {
        let records: Vec<Value> = self.store.iter().map(|(_, r)| Value::Object(r)).collect();
        *self = Index::new(settings);
        for record in records {
            self.add(record).expect("stored records are valid");
        }
    }

    /// Removes every record but keeps the settings.
    pub fn clear(&mut self) {
        *self = Index::new(self.settings.clone());
    }

    /// The stored record with this objectID.
    pub fn get_object(&self, object_id: &str) -> Option<Map<String, Value>> {
        self.store.get(self.store.id_of(object_id)?)
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

    /// Writes the built index in a binary form that `read_image` loads
    /// without re-indexing any record.
    pub fn write_image(&self, out: impl std::io::Write) -> std::io::Result<()> {
        // Settings use serde features (flatten) that bincode cannot read
        // back, so they travel as JSON.
        let settings = serde_json::to_string(&self.settings).expect("settings serialize");
        bincode::serialize_into(out, &(IMAGE_VERSION, settings, self)).map_err(std::io::Error::other)
    }

    /// Reads an image written by `write_image` by the same engine version.
    pub fn read_image(input: impl std::io::Read) -> std::io::Result<Index> {
        let invalid = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
        let (version, settings, mut index): (u32, String, Index) =
            bincode::deserialize_from(input).map_err(|e| invalid(e.to_string()))?;
        if version != IMAGE_VERSION {
            return Err(invalid(format!("image version {version}, expected {IMAGE_VERSION}")));
        }
        index.settings = serde_json::from_str(&settings).map_err(|e| invalid(e.to_string()))?;
        index.synonym_map = synonyms::build_map(&index.settings.synonyms, index.segmentation());
        index.word_ids = index.words.iter().map(|(w, &id)| (w.clone(), id)).collect();
        Ok(index)
    }

    /// Records containing exactly this (normalized) word.
    pub fn word(&self, word: &str) -> Option<&Postings> {
        let id = *self.word_ids.get(word)?;
        self.postings[id as usize].as_ref()
    }

    /// Records containing a word that starts with this (normalized) prefix.
    /// Short prefixes are precomputed; longer ones are the union of the few
    /// words starting with them.
    pub fn prefix_docs(&self, prefix: &str) -> Option<Cow<'_, RoaringBitmap>> {
        if prefix.chars().count() <= MAX_STORED_PREFIX {
            return self.prefixes.get(prefix).map(|p| Cow::Borrowed(&p.docs));
        }
        let mut docs = RoaringBitmap::new();
        let mut any = false;
        for id in self.word_ids_with_prefix(prefix) {
            if let Some(postings) = &self.postings[id as usize] {
                docs |= &postings.docs;
                any = true;
            }
        }
        any.then_some(Cow::Owned(docs))
    }

    /// A record's words in reading order (empty for deleted records).
    pub fn occurrences(&self, doc: DocId) -> &[Occurrence] {
        self.occurrences.get(doc as usize).map_or(&[], Vec::as_slice)
    }

    /// One more than the largest word id handed out so far.
    pub fn word_id_limit(&self) -> usize {
        self.next_word_id as usize
    }

    /// Records containing a word that starts with `prefix`, per attribute.
    pub fn prefix_docs_by_attribute(&self, prefix: &str) -> Cow<'_, [RoaringBitmap]> {
        if prefix.chars().count() <= MAX_STORED_PREFIX {
            return Cow::Borrowed(self.prefixes.get(prefix).map_or(&[], |p| p.by_attribute.as_slice()));
        }
        let mut sets: Vec<RoaringBitmap> = Vec::new();
        for id in self.word_ids_with_prefix(prefix) {
            let Some(postings) = &self.postings[id as usize] else { continue };
            if sets.len() < postings.by_attribute.len() {
                sets.resize_with(postings.by_attribute.len(), RoaringBitmap::new);
            }
            for (set, docs) in sets.iter_mut().zip(&postings.by_attribute) {
                *set |= docs;
            }
        }
        Cow::Owned(sets)
    }

    /// Ids of the dictionary words starting with `prefix`.
    pub fn word_ids_with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = WordId> + 'a {
        self.words
            .range::<str, _>((Bound::Included(prefix), Bound::Unbounded))
            .take_while(move |(w, _)| w.starts_with(prefix))
            .map(|(_, &id)| id)
    }

    /// Dictionary words starting with `prefix`, in sorted order.
    pub fn words_with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.words
            .range::<str, _>((Bound::Included(prefix), Bound::Unbounded))
            .map(|(w, _)| w.as_str())
            .take_while(move |w| w.starts_with(prefix))
    }

    /// Number of live records.
    pub fn all_docs_len(&self) -> u64 {
        self.live.len()
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

    /// The most frequent values among `candidates`, and the stats of the
    /// numeric ones.
    pub fn facet_counts(
        &self,
        attribute: &str,
        candidates: &RoaringBitmap,
        limit: usize,
    ) -> (Vec<FacetCount>, Option<FacetStats>) {
        self.facets.counts(attribute, candidates, limit)
    }

    pub fn geo(&self) -> &GeoIndex {
        &self.geo
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
            hits: self.occurrences.iter().map(Vec::len).sum(),
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

    /// Value of `customRanking` rule `rule` for a record, if it has one.
    pub fn custom_value(&self, rule: usize, doc: DocId) -> Option<f64> {
        let value = *self.custom_columns.get(rule)?.get(doc as usize)?;
        (!value.is_nan()).then_some(value)
    }

    /// The record's `customRanking` key. Rules beyond MAX_ORDERED_RULES are
    /// not part of it.
    pub fn custom_key(&self, doc: DocId) -> CustomKey {
        let rules = &self.settings.custom_ranking;
        std::array::from_fn(|i| match (rules.get(i), self.custom_value(i, doc)) {
            (None, _) => 0,
            (Some(rule), Some(v)) => {
                let key = ordered(v);
                if rule.descending { u64::MAX - 1 - key } else { key }
            }
            // Missing values sort last in either direction.
            (Some(_), None) => u64::MAX,
        })
    }

    /// Live records in `customRanking` order, with their keys; `None` when
    /// the order is not kept (no rules, or more than MAX_ORDERED_RULES).
    pub fn custom_order(&self) -> Option<impl Iterator<Item = &(CustomKey, DocId)>> {
        self.keeps_custom_order().then(|| self.custom_order.iter())
    }

    fn keeps_custom_order(&self) -> bool {
        (1..=MAX_ORDERED_RULES).contains(&self.settings.custom_ranking.len())
    }

    fn index(&mut self, id: DocId, doc: &Map<String, Value>) {
        self.live.insert(id);
        for (rule, column) in self.settings.custom_ranking.iter().zip(&mut self.custom_columns) {
            if column.len() <= id as usize {
                column.resize(id as usize + 1, f64::NAN);
            }
            column[id as usize] = doc.get(&rule.attribute).and_then(Value::as_f64).unwrap_or(f64::NAN);
        }
        if self.keeps_custom_order() {
            self.custom_order.insert((self.custom_key(id), id));
        }
        self.facets.add(id, doc);
        self.numbers.add(id, doc);
        self.geo.add(id, doc);
        let mut hits: FxHashMap<String, Vec<Hit>> = FxHashMap::default();
        let segmentation = self.segmentation().with_parts();
        for (attribute, value) in self.searchable_fields(doc) {
            for token in tokenize_value(value, segmentation) {
                hits.entry(token.text).or_default().push(Hit { attribute, position: token.position });
            }
        }

        // Attributes in which each prefix of this record's words occurs.
        let mut prefixes: FxHashMap<&str, AttributeSet> = FxHashMap::default();
        for (word, word_hits) in &hits {
            for prefix in prefixes_of(word).take(MAX_STORED_PREFIX) {
                let set = prefixes.entry(prefix).or_default();
                for h in word_hits {
                    set.insert(h.attribute);
                }
            }
        }
        for (prefix, attributes) in prefixes {
            if !self.prefixes.contains_key(prefix) {
                self.prefixes.insert(prefix.to_owned(), PrefixDocs::default());
            }
            let entry = self.prefixes.get_mut(prefix).expect("just ensured");
            entry.docs.insert(id);
            for attribute in attributes.iter() {
                insert_in_attribute(&mut entry.by_attribute, attribute, id);
            }
        }
        let mut occurrences = Vec::with_capacity(hits.values().map(Vec::len).sum());
        for (word, word_hits) in hits {
            let word_id = match self.word_ids.get(&word) {
                Some(&word_id) => word_id,
                None => {
                    let word_id = self.next_word_id;
                    self.next_word_id += 1;
                    self.words.insert(word.clone(), word_id);
                    self.word_ids.insert(word.clone(), word_id);
                    self.postings.push(Some(Postings {
                        id: word_id,
                        word,
                        docs: RoaringBitmap::new(),
                        by_attribute: Vec::new(),
                    }));
                    word_id
                }
            };
            let postings = self.postings[word_id as usize].as_mut().expect("live word");
            postings.docs.insert(id);
            for h in &word_hits {
                insert_in_attribute(&mut postings.by_attribute, h.attribute, id);
                occurrences.push(Occurrence { word: word_id, attribute: h.attribute, position: h.position });
            }
        }
        occurrences.sort_unstable_by_key(|o| (o.attribute, o.position));
        if self.occurrences.len() <= id as usize {
            self.occurrences.resize_with(id as usize + 1, Vec::new);
        }
        self.occurrences[id as usize] = occurrences;
    }

    /// Removes a record's postings. The old record is re-tokenized to find its
    /// words, so no per-record word list has to be kept in memory.
    fn unindex(&mut self, id: DocId, old: &Map<String, Value>) {
        self.live.remove(id);
        if self.keeps_custom_order() {
            // The columns still hold the old values here.
            self.custom_order.remove(&(self.custom_key(id), id));
        }
        for column in &mut self.custom_columns {
            if let Some(value) = column.get_mut(id as usize) {
                *value = f64::NAN;
            }
        }
        self.facets.remove(id);
        self.numbers.remove(id, old);
        self.geo.remove(id);
        // The record's word list says which words to remove it from.
        let old_words: FxHashSet<WordId> = self
            .occurrences
            .get_mut(id as usize)
            .map(std::mem::take)
            .unwrap_or_default()
            .iter()
            .map(|o| o.word)
            .collect();
        // Removing the record from every prefix of every word it had is safe:
        // the whole record is going away, so no other word of it still needs them.
        let mut prefixes: FxHashSet<String> = FxHashSet::default();
        for word_id in old_words {
            let slot = &mut self.postings[word_id as usize];
            let Some(postings) = slot.as_mut() else { continue };
            prefixes.extend(prefixes_of(&postings.word).take(MAX_STORED_PREFIX).map(str::to_owned));
            postings.docs.remove(id);
            for set in &mut postings.by_attribute {
                set.remove(id);
            }
            if postings.docs.is_empty() {
                let word = slot.take().expect("checked").word;
                self.words.remove(&word);
                self.word_ids.remove(&word);
            }
        }
        for prefix in &prefixes {
            if let Some(entry) = self.prefixes.get_mut(prefix.as_str()) {
                entry.docs.remove(id);
                for set in &mut entry.by_attribute {
                    set.remove(id);
                }
                if entry.docs.is_empty() {
                    self.prefixes.remove(prefix.as_str());
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
        let hits_of = |doc| -> Vec<Hit> {
            index.occurrences(doc).iter().filter(|o| o.word == wireless.id).map(Occurrence::hit).collect()
        };
        assert_eq!(hits_of(0), [Hit { attribute: 0, position: 0 }]);
        assert_eq!(hits_of(1), [Hit { attribute: 1, position: 1 }]);
        // Only searchable attributes are indexed.
        assert!(index.word("99").is_none());
    }

    #[test]
    fn prefix_lookup_covers_all_words_with_that_start() {
        let index = sample();
        assert_eq!(ids(&index.prefix_docs("head").unwrap()), [0, 1]);
        assert_eq!(ids(&index.prefix_docs("heads").unwrap()), [1]);
        assert_eq!(ids(&index.prefix_docs("headphones").unwrap()), [0]);
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
        assert_eq!(ids(&index.prefix_docs("head").unwrap()), [0]);
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
        let fast = index.word("fast").unwrap().id;
        let positions: Vec<u32> = index.occurrences(0).iter().filter(|o| o.word == fast).map(|o| o.position).collect();
        assert_eq!(positions, [0, 2]);
        // Every word of the record, in reading order.
        assert_eq!(index.occurrences(0).iter().map(|o| o.position).collect::<Vec<_>>(), [0, 1, 2, 3]);
        assert_eq!(index.stats().hits, 4);
    }

    #[test]
    fn set_settings_reindexes_and_clear_keeps_settings() {
        let mut index = sample();
        assert!(index.word("water").is_some());
        index.set_settings(Settings { searchable_attributes: Some(vec!["name".into()]), ..Settings::default() });
        assert!(index.word("water").is_none());
        assert_eq!(ids(&index.word("kettle").unwrap().docs), [2]);
        assert_eq!(index.get_object("3").unwrap()["description"], "Boils water fast");
        index.clear();
        assert_eq!(index.stats().records, 0);
        assert_eq!(index.attributes(), ["name"]);
        assert!(index.get_object("3").is_none());
    }

    #[test]
    fn custom_order_follows_writes() {
        let settings: Settings =
            serde_json::from_value(json!({"customRanking": ["desc(rating)", "asc(price)"]})).unwrap();
        let mut index = Index::new(settings);
        for (id, rating, price) in [("a", 3.0, 10), ("b", 5.0, 30), ("c", 5.0, 20)] {
            index.add(json!({"objectID": id, "rating": rating, "price": price})).unwrap();
        }
        index.add(json!({"objectID": "d"})).unwrap();
        let order = |index: &Index| -> Vec<String> {
            index
                .custom_order()
                .unwrap()
                .map(|(_, doc)| index.store().get(*doc).unwrap()["objectID"].as_str().unwrap().to_owned())
                .collect()
        };
        // Highest rating first, cheaper first on ties, missing values last.
        assert_eq!(order(&index), ["c", "b", "a", "d"]);
        index.add(json!({"objectID": "a", "rating": 9, "price": 1})).unwrap();
        index.delete("c");
        assert_eq!(order(&index), ["a", "b", "d"]);
        assert!(Index::new(Settings::default()).custom_order().is_none());
    }

    #[test]
    fn an_image_reloads_to_the_same_index() {
        use crate::search::{SearchParams, search};
        let settings: Settings = serde_json::from_value(json!({
            "searchableAttributes": ["name", "brand"],
            "customRanking": ["desc(rating)"],
            "attributesForFaceting": ["brand", "rating"],
            "synonyms": [{"type": "synonym", "synonyms": ["phone", "mobile"]}],
            "ignorePlurals": true
        }))
        .unwrap();
        let mut index = Index::new(settings);
        let brands = ["Orbix", "Lumen", "Vexa"];
        let nouns = ["phone", "kettle", "lamp", "headphones", "stand"];
        for i in 0..300 {
            let name = format!("{} {} model {}", brands[i % 3], nouns[i % 5], i % 17);
            index.add(json!({"objectID": i.to_string(), "name": name, "brand": brands[i % 3], "rating": i % 5})).unwrap();
        }
        for i in (0..300).step_by(7) {
            index.delete(&i.to_string());
        }
        let mut image = Vec::new();
        index.write_image(&mut image).unwrap();
        let mut reloaded = Index::read_image(&image[..]).unwrap();
        assert_eq!(reloaded.stats(), index.stats());

        let same = |a: &Index, b: &Index| {
            for query in ["", "orbix", "mobile", "lamps", "ketle", "head", "vexa st", "model 3"] {
                let mut params = SearchParams::new(query);
                params.facets = vec!["brand".into()];
                params.filters = Some("rating >= 2".into());
                let (x, y) = (search(a, &params).unwrap(), search(b, &params).unwrap());
                assert_eq!(x.nb_hits, y.nb_hits, "{query}");
                assert_eq!(format!("{:?}{:?}", x.hits, x.facets), format!("{:?}{:?}", y.hits, y.facets), "{query}");
            }
        };
        same(&index, &reloaded);
        // Both keep accepting the same writes with the same results.
        for target in [&mut index, &mut reloaded] {
            target.add(json!({"objectID": "7", "name": "Lumen phone stand", "brand": "Lumen", "rating": 4})).unwrap();
            target.add(json!({"objectID": "1000", "name": "Brand new kettle", "brand": "Zeta", "rating": 3})).unwrap();
            target.delete("2");
        }
        same(&index, &reloaded);

        assert!(Index::read_image(&image[..image.len() / 2]).is_err());
    }
}
