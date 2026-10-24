//! Facet and numeric indexes, used by filters and facet counts.
//!
//! - Facets: for each attribute in `attributesForFaceting`, a map from value
//!   to the set of records having it. Values are matched case- and
//!   accent-insensitively; counts show the value as first seen.
//! - Numbers: every numeric top-level attribute (or array of numbers) is kept
//!   in a sorted set of (value, record), so a range filter is one ordered scan.

use crate::store::DocId;
use crate::tokenizer::normalize;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::ops::Bound;

/// One `attributesForFaceting` entry: `brand`, or `filterOnly(brand)` to allow
/// filtering without computing counts.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct FacetAttribute {
    pub attribute: String,
    pub filter_only: bool,
}

impl TryFrom<String> for FacetAttribute {
    type Error = String;

    fn try_from(entry: String) -> Result<Self, Self::Error> {
        let inner = |prefix: &str| entry.strip_prefix(prefix).and_then(|r| r.strip_suffix(')'));
        let (attribute, filter_only) = if let Some(a) = inner("filterOnly(") {
            (a, true)
        } else if let Some(a) = inner("searchable(") {
            (a, false)
        } else {
            (entry.as_str(), false)
        };
        if attribute.is_empty() || attribute.contains(['(', ')']) {
            return Err(format!("invalid attributesForFaceting entry {entry:?}"));
        }
        Ok(FacetAttribute { attribute: attribute.to_owned(), filter_only })
    }
}

impl From<FacetAttribute> for String {
    fn from(f: FacetAttribute) -> String {
        if f.filter_only { format!("filterOnly({})", f.attribute) } else { f.attribute }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FacetCount {
    pub value: String,
    pub count: u64,
}

#[derive(Debug, Default)]
struct FacetValue {
    display: String,
    docs: RoaringBitmap,
}

/// One facet attribute. Values get permanent ids; each record keeps the ids
/// of its values, so counts can be taken record by record when that is
/// cheaper than one intersection per value.
#[derive(Debug, Default)]
struct FacetValues {
    ids: HashMap<String, u32>,
    values: Vec<FacetValue>,
    /// Indexed by DocId.
    doc_values: Vec<Vec<u32>>,
}

/// Counting record by record is used when there are fewer than this many
/// matching records per distinct value.
const RECORDS_PER_VALUE_FOR_RECORD_COUNTING: u64 = 32;

#[derive(Debug, Default)]
pub struct FacetIndex {
    attributes: HashMap<String, FacetValues>,
}

/// Text forms of a facet value: strings, numbers and booleans; arrays give
/// one value per element.
fn facet_values(value: &Value) -> Vec<String> {
    fn scalar(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }
    match value {
        Value::Array(items) => items.iter().filter_map(scalar).collect(),
        v => scalar(v).into_iter().collect(),
    }
}

fn facet_key(value: &str) -> String {
    normalize(value.trim())
}

impl FacetIndex {
    pub fn new(attributes: &[FacetAttribute]) -> Self {
        FacetIndex { attributes: attributes.iter().map(|f| (f.attribute.clone(), FacetValues::default())).collect() }
    }

    pub fn contains(&self, attribute: &str) -> bool {
        self.attributes.contains_key(attribute)
    }

    pub fn add(&mut self, id: DocId, doc: &Map<String, Value>) {
        for (attribute, facet) in &mut self.attributes {
            let Some(value) = doc.get(attribute) else { continue };
            let mut ids = Vec::new();
            for display in facet_values(value) {
                let key = facet_key(&display);
                let value_id = match facet.ids.get(&key) {
                    Some(&v) => v,
                    None => {
                        let v = facet.values.len() as u32;
                        facet.ids.insert(key, v);
                        facet.values.push(FacetValue::default());
                        v
                    }
                };
                let entry = &mut facet.values[value_id as usize];
                if entry.docs.is_empty() {
                    entry.display = display;
                }
                entry.docs.insert(id);
                ids.push(value_id);
            }
            ids.sort_unstable();
            ids.dedup();
            if facet.doc_values.len() <= id as usize {
                facet.doc_values.resize_with(id as usize + 1, Vec::new);
            }
            facet.doc_values[id as usize] = ids;
        }
    }

    pub fn remove(&mut self, id: DocId) {
        for facet in self.attributes.values_mut() {
            let Some(ids) = facet.doc_values.get_mut(id as usize) else { continue };
            for value_id in std::mem::take(ids) {
                facet.values[value_id as usize].docs.remove(id);
            }
        }
    }

    pub fn docs(&self, attribute: &str, value: &str) -> RoaringBitmap {
        let Some(facet) = self.attributes.get(attribute) else { return RoaringBitmap::new() };
        facet.ids.get(&facet_key(value)).map(|&v| facet.values[v as usize].docs.clone()).unwrap_or_default()
    }

    /// Value counts among `candidates`, most frequent first (ties by value),
    /// at most `limit` values.
    pub fn counts(&self, attribute: &str, candidates: &RoaringBitmap, limit: usize) -> Vec<FacetCount> {
        let Some(facet) = self.attributes.get(attribute) else { return Vec::new() };
        let few_records = candidates.len() < RECORDS_PER_VALUE_FOR_RECORD_COUNTING * facet.values.len() as u64;
        let raw: Vec<(u32, u64)> = if few_records {
            let mut counts = vec![0u64; facet.values.len()];
            for doc in candidates {
                for &v in facet.doc_values.get(doc as usize).map_or(&[][..], Vec::as_slice) {
                    counts[v as usize] += 1;
                }
            }
            counts.into_iter().enumerate().filter(|&(_, c)| c > 0).map(|(v, c)| (v as u32, c)).collect()
        } else {
            facet
                .values
                .iter()
                .enumerate()
                .map(|(v, value)| (v as u32, value.docs.intersection_len(candidates)))
                .filter(|&(_, c)| c > 0)
                .collect()
        };
        let mut counts: Vec<FacetCount> = raw
            .into_iter()
            .map(|(v, count)| FacetCount { value: facet.values[v as usize].display.clone(), count })
            .collect();
        counts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));
        counts.truncate(limit);
        counts
    }
}

/// An f64 with a total order, so it can be a `BTreeSet` key.
#[derive(Debug, Clone, Copy)]
struct Number(f64);

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0) == Ordering::Equal
    }
}
impl Eq for Number {}
impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Number {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

#[derive(Debug, Default)]
pub struct NumericIndex {
    attributes: HashMap<String, BTreeSet<(Number, DocId)>>,
}

fn numbers(value: &Value) -> Vec<f64> {
    match value {
        // -0.0 and 0.0 must compare equal in the total order.
        Value::Number(n) => n.as_f64().map(|x| x + 0.0).into_iter().collect(),
        Value::Array(items) => items.iter().filter_map(Value::as_f64).map(|x| x + 0.0).collect(),
        _ => Vec::new(),
    }
}

impl NumericIndex {
    pub fn add(&mut self, id: DocId, doc: &Map<String, Value>) {
        for (attribute, value) in doc {
            for n in numbers(value) {
                self.attributes.entry(attribute.clone()).or_default().insert((Number(n), id));
            }
        }
    }

    pub fn remove(&mut self, id: DocId, doc: &Map<String, Value>) {
        for (attribute, value) in doc {
            let Some(set) = self.attributes.get_mut(attribute) else { continue };
            for n in numbers(value) {
                set.remove(&(Number(n), id));
            }
            if set.is_empty() {
                self.attributes.remove(attribute);
            }
        }
    }

    /// Records with at least one value of `attribute` inside the bounds.
    pub fn docs(&self, attribute: &str, low: Bound<f64>, high: Bound<f64>) -> RoaringBitmap {
        let Some(set) = self.attributes.get(attribute) else { return RoaringBitmap::new() };
        let low = match low {
            Bound::Included(x) => Bound::Included((Number(x + 0.0), DocId::MIN)),
            Bound::Excluded(x) => Bound::Excluded((Number(x + 0.0), DocId::MAX)),
            Bound::Unbounded => Bound::Unbounded,
        };
        let high = match high {
            Bound::Included(x) => Bound::Included((Number(x + 0.0), DocId::MAX)),
            Bound::Excluded(x) => Bound::Excluded((Number(x + 0.0), DocId::MIN)),
            Bound::Unbounded => Bound::Unbounded,
        };
        if is_empty_range(&low, &high) {
            return RoaringBitmap::new();
        }
        set.range((low, high)).map(|(_, id)| *id).collect()
    }
}

/// `BTreeSet::range` panics when the start is after the end.
fn is_empty_range<T: Ord>(low: &Bound<T>, high: &Bound<T>) -> bool {
    match (low, high) {
        (Bound::Included(l), Bound::Included(h)) => l > h,
        (Bound::Included(l), Bound::Excluded(h))
        | (Bound::Excluded(l), Bound::Included(h))
        | (Bound::Excluded(l), Bound::Excluded(h)) => l >= h,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::ops::Bound::{Excluded, Included, Unbounded};

    fn doc(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn ids(b: &RoaringBitmap) -> Vec<u32> {
        b.iter().collect()
    }

    #[test]
    fn parses_facet_attribute_modifiers() {
        let parse = |s: &str| FacetAttribute::try_from(s.to_owned());
        assert_eq!(parse("brand").unwrap(), FacetAttribute { attribute: "brand".into(), filter_only: false });
        assert!(parse("filterOnly(author)").unwrap().filter_only);
        assert!(!parse("searchable(brand)").unwrap().filter_only);
        assert!(parse("filterOnly()").is_err());
        assert!(parse("weird(x)").is_err());
    }

    #[test]
    fn facet_values_match_case_and_accent_insensitively() {
        let mut f = FacetIndex::new(&[FacetAttribute::try_from("brand".to_owned()).unwrap()]);
        f.add(0, &doc(json!({"brand": "Élan"})));
        f.add(1, &doc(json!({"brand": "elan"})));
        f.add(2, &doc(json!({"brand": "Orbix"})));
        assert_eq!(ids(&f.docs("brand", "ELAN")), [0, 1]);
        let counts = f.counts("brand", &(0..3).collect(), 10);
        assert_eq!(counts[0], FacetCount { value: "Élan".into(), count: 2 });
        f.remove(2);
        assert_eq!(f.counts("brand", &(0..3).collect(), 10).len(), 1);
    }

    #[test]
    fn arrays_numbers_and_booleans_are_facet_values() {
        let attrs: Vec<FacetAttribute> =
            ["genres", "in_stock", "year"].iter().map(|s| FacetAttribute::try_from(s.to_string()).unwrap()).collect();
        let mut f = FacetIndex::new(&attrs);
        f.add(0, &doc(json!({"genres": ["Drama", "Horror"], "in_stock": true, "year": 1996})));
        f.add(1, &doc(json!({"genres": ["Drama"], "in_stock": false, "year": 2001})));
        assert_eq!(ids(&f.docs("genres", "drama")), [0, 1]);
        assert_eq!(ids(&f.docs("in_stock", "true")), [0]);
        assert_eq!(ids(&f.docs("year", "1996")), [0]);
        let only_first: RoaringBitmap = [0].into_iter().collect();
        let counts = f.counts("genres", &only_first, 10);
        assert_eq!(counts.iter().map(|c| (c.value.as_str(), c.count)).collect::<Vec<_>>(), [("Drama", 1), ("Horror", 1)]);
    }

    #[test]
    fn counts_are_sorted_and_limited() {
        let mut f = FacetIndex::new(&[FacetAttribute::try_from("c".to_owned()).unwrap()]);
        for (id, c) in ["b", "a", "b", "c", "c", "c"].iter().enumerate() {
            f.add(id as u32, &doc(json!({ "c": c })));
        }
        let counts = f.counts("c", &(0..6).collect(), 2);
        assert_eq!(counts.iter().map(|c| (c.value.as_str(), c.count)).collect::<Vec<_>>(), [("c", 3), ("b", 2)]);
    }

    #[test]
    fn numeric_ranges_respect_bounds() {
        let mut n = NumericIndex::default();
        for (id, price) in [10.0, 20.0, 20.0, 35.5, -0.0].iter().enumerate() {
            n.add(id as u32, &doc(json!({ "price": price })));
        }
        n.add(5, &doc(json!({"price": [5, 50]})));
        assert_eq!(ids(&n.docs("price", Included(20.0), Included(35.5))), [1, 2, 3]);
        assert_eq!(ids(&n.docs("price", Excluded(20.0), Unbounded)), [3, 5]);
        assert_eq!(ids(&n.docs("price", Unbounded, Excluded(10.0))), [4, 5]);
        assert_eq!(ids(&n.docs("price", Included(0.0), Included(0.0))), [4]);
        assert!(n.docs("price", Included(30.0), Included(20.0)).is_empty());
        assert!(n.docs("price", Excluded(20.0), Excluded(20.0)).is_empty());
        assert!(n.docs("missing", Unbounded, Unbounded).is_empty());
        n.remove(5, &doc(json!({"price": [5, 50]})));
        assert_eq!(ids(&n.docs("price", Included(40.0), Unbounded)), Vec::<u32>::new());
    }

    #[test]
    fn counting_by_record_and_by_value_agree() {
        let mut f = FacetIndex::new(&[FacetAttribute::try_from("tag".to_owned()).unwrap()]);
        let mut seed = 11u64;
        let mut next = |n: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        for id in 0..3000u32 {
            let tags: Vec<String> = (0..next(4)).map(|_| format!("t{}", next(200))).collect();
            f.add(id, &doc(json!({ "tag": tags })));
        }
        for id in (0..3000).step_by(7) {
            f.remove(id);
        }
        for size in [1u64, 10, 100, 1000, 3000] {
            let candidates: RoaringBitmap = (0..3000u32).filter(|_| next(3000) < size).collect();
            // The two strategies, forced: by record (huge threshold) and by value.
            let facet = &f.attributes["tag"];
            let by_value: Vec<(String, u64)> = {
                let mut c: Vec<(String, u64)> = facet
                    .values
                    .iter()
                    .map(|v| (v.display.clone(), v.docs.intersection_len(&candidates)))
                    .filter(|&(_, n)| n > 0)
                    .collect();
                c.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                c
            };
            let counted: Vec<(String, u64)> =
                f.counts("tag", &candidates, usize::MAX).into_iter().map(|c| (c.value, c.count)).collect();
            assert_eq!(counted, by_value, "candidates {}", candidates.len());
        }
    }
}

