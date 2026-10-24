//! Facet and numeric indexes, used by filters and facet counts.
//!
//! - Facets: for each attribute in `attributesForFaceting`, a map from value
//!   to the set of records having it. Values are matched case- and
//!   accent-insensitively; counts show the value as first seen.
//! - Numbers: every numeric top-level attribute (or array of numbers) is kept
//!   in a sorted set of (value, record), so a range filter is one ordered scan.

use crate::store::DocId;
use crate::index::ordered;
use crate::tokenizer::normalize;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
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

#[derive(Debug, Default, Deserialize, Serialize)]
struct FacetValue {
    display: String,
    /// The value as a number, when records give it as a JSON number.
    number: Option<f64>,
    docs: RoaringBitmap,
}

/// Algolia's `facets_stats` for a numeric facet: over the matching records'
/// values (each value counted once per record that has it).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct FacetStats {
    pub min: f64,
    pub max: f64,
    pub avg: f64,
    pub sum: f64,
}

/// One facet attribute. Values get permanent ids; each record keeps the ids
/// of its values, so counts can be taken record by record when that is
/// cheaper than one intersection per value.
#[derive(Debug, Default, Deserialize, Serialize)]
struct FacetValues {
    ids: HashMap<String, u32>,
    values: Vec<FacetValue>,
    /// Indexed by DocId.
    doc_values: Vec<Vec<u32>>,
}

/// Counting record by record is used when there are fewer than this many
/// matching records per distinct value.
const RECORDS_PER_VALUE_FOR_RECORD_COUNTING: u64 = 32;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct FacetIndex {
    attributes: HashMap<String, FacetValues>,
}

/// Text forms of a facet value (with the number, for JSON numbers):
/// strings, numbers and booleans; arrays give one value per element.
fn facet_values(value: &Value) -> Vec<(String, Option<f64>)> {
    fn scalar(v: &Value) -> Option<(String, Option<f64>)> {
        match v {
            Value::String(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()).map(|s| (s, None)),
            Value::Number(n) => Some((n.to_string(), n.as_f64())),
            Value::Bool(b) => Some((b.to_string(), None)),
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
            for (display, number) in facet_values(value) {
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
                entry.number = entry.number.or(number);
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
    /// at most `limit` values, and the stats of its numeric values when
    /// there are some.
    pub fn counts(&self, attribute: &str, candidates: &RoaringBitmap, limit: usize) -> (Vec<FacetCount>, Option<FacetStats>) {
        let Some(facet) = self.attributes.get(attribute) else { return (Vec::new(), None) };
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
        let mut stats: Option<FacetStats> = None;
        let mut weight = 0u64;
        for &(v, count) in &raw {
            let Some(n) = facet.values[v as usize].number else { continue };
            weight += count;
            let s = stats.get_or_insert(FacetStats { min: n, max: n, avg: 0.0, sum: 0.0 });
            s.min = s.min.min(n);
            s.max = s.max.max(n);
            s.sum += n * count as f64;
        }
        if let Some(s) = &mut stats {
            s.avg = s.sum / weight as f64;
        }
        let mut counts: Vec<FacetCount> = raw
            .into_iter()
            .map(|(v, count)| FacetCount { value: facet.values[v as usize].display.clone(), count })
            .collect();
        counts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));
        counts.truncate(limit);
        (counts, stats)
    }
}

/// An f64 with a total order, so it can be a `BTreeSet` key.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
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

/// Record sets for ranges of values at three levels of detail, so a range
/// filter unions a few ready-made sets instead of collecting every record.
/// A level's bucket is `key >> shift`, where `key` is the value mapped to
/// a u64 in the same order (`ordered`).
const LEVEL_SHIFTS: [u32; 3] = [56, 48, 40];

#[derive(Debug, Default, Deserialize, Serialize)]
struct NumericValues {
    /// Every (value, record) pair, for exact edges and removals.
    values: BTreeSet<(Number, DocId)>,
    /// One map per level: bucket -> records with a value in it.
    levels: [BTreeMap<u64, RoaringBitmap>; 3],
}

impl NumericValues {
    fn insert(&mut self, n: f64, id: DocId) {
        if self.values.insert((Number(n), id)) {
            let key = ordered(n);
            for (level, shift) in self.levels.iter_mut().zip(LEVEL_SHIFTS) {
                level.entry(key >> shift).or_default().insert(id);
            }
        }
    }

    fn remove(&mut self, n: f64, id: DocId) {
        if !self.values.remove(&(Number(n), id)) {
            return;
        }
        let key = ordered(n);
        // The record may have another value in the same bucket (arrays).
        for (level, shift) in self.levels.iter_mut().zip(LEVEL_SHIFTS) {
            let bucket = key >> shift;
            let start = unordered(bucket << shift);
            let end = unordered((bucket << shift) | ((1u64 << shift) - 1));
            let still_there = self
                .values
                .range((Number(start), DocId::MIN)..=(Number(end), DocId::MAX))
                .any(|&(_, d)| d == id);
            if !still_there && let Some(docs) = level.get_mut(&bucket) {
                docs.remove(id);
                if docs.is_empty() {
                    level.remove(&bucket);
                }
            }
        }
    }

    /// Records with a value whose key is in `lo..=hi`, using level `depth`
    /// and finer ones.
    fn collect(&self, lo: u64, hi: u64, depth: usize, out: &mut RoaringBitmap) {
        if lo > hi {
            return;
        }
        let Some(&shift) = LEVEL_SHIFTS.get(depth) else {
            // Finer than every level: scan the values themselves.
            let range = (Number(unordered(lo)), DocId::MIN)..=(Number(unordered(hi)), DocId::MAX);
            out.extend(self.values.range(range).map(|&(_, d)| d));
            return;
        };
        let size = 1u64 << shift;
        let first_full = if lo.is_multiple_of(size) { lo / size } else { lo / size + 1 };
        let last_full = if hi % size == size - 1 { hi / size } else { (hi / size).wrapping_sub(1) };
        if hi / size < first_full || last_full == u64::MAX || first_full > last_full {
            // No whole bucket fits: refine.
            self.collect(lo, hi, depth + 1, out);
            return;
        }
        for (_, docs) in self.levels[depth].range(first_full..=last_full) {
            *out |= docs;
        }
        // The partial buckets at both ends, at the next level.
        if lo < first_full * size {
            self.collect(lo, first_full * size - 1, depth + 1, out);
        }
        let end_of_full = last_full * size + (size - 1);
        if hi > end_of_full {
            self.collect(end_of_full + 1, hi, depth + 1, out);
        }
    }
}

/// The inverse of `ordered`.
fn unordered(key: u64) -> f64 {
    let bits = if key >> 63 == 1 { key & !(1 << 63) } else { !key };
    f64::from_bits(bits)
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct NumericIndex {
    attributes: HashMap<String, NumericValues>,
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
                if !self.attributes.contains_key(attribute) {
                    self.attributes.insert(attribute.clone(), NumericValues::default());
                }
                self.attributes.get_mut(attribute).expect("just ensured").insert(n, id);
            }
        }
    }

    pub fn remove(&mut self, id: DocId, doc: &Map<String, Value>) {
        for (attribute, value) in doc {
            let Some(values) = self.attributes.get_mut(attribute) else { continue };
            for n in numbers(value) {
                values.remove(n, id);
            }
            if values.values.is_empty() {
                self.attributes.remove(attribute);
            }
        }
    }

    /// Records with at least one value of `attribute` inside the bounds.
    pub fn docs(&self, attribute: &str, low: Bound<f64>, high: Bound<f64>) -> RoaringBitmap {
        let Some(values) = self.attributes.get(attribute) else { return RoaringBitmap::new() };
        // Inclusive bounds in key space.
        let lo = match low {
            Bound::Included(x) => ordered(x + 0.0),
            Bound::Excluded(x) => match ordered(x + 0.0).checked_add(1) {
                Some(k) => k,
                None => return RoaringBitmap::new(),
            },
            Bound::Unbounded => 0,
        };
        let hi = match high {
            Bound::Included(x) => ordered(x + 0.0),
            Bound::Excluded(x) => match ordered(x + 0.0).checked_sub(1) {
                Some(k) => k,
                None => return RoaringBitmap::new(),
            },
            Bound::Unbounded => u64::MAX - 2,
        };
        let mut out = RoaringBitmap::new();
        values.collect(lo, hi, 0, &mut out);
        out
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
        let (counts, stats) = f.counts("brand", &(0..3).collect(), 10);
        assert_eq!(counts[0], FacetCount { value: "Élan".into(), count: 2 });
        assert_eq!(stats, None);
        f.remove(2);
        assert_eq!(f.counts("brand", &(0..3).collect(), 10).0.len(), 1);
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
        let (counts, _) = f.counts("genres", &only_first, 10);
        assert_eq!(counts.iter().map(|c| (c.value.as_str(), c.count)).collect::<Vec<_>>(), [("Drama", 1), ("Horror", 1)]);
    }

    #[test]
    fn numeric_facets_have_stats_over_the_candidates() {
        let mut f = FacetIndex::new(&[FacetAttribute::try_from("price".to_owned()).unwrap()]);
        for (id, price) in [json!(10), json!(20.5), json!(20.5), json!([5, 100]), json!("n/a")].into_iter().enumerate() {
            f.add(id as u32, &doc(json!({ "price": price })));
        }
        let (_, stats) = f.counts("price", &(0..5).collect(), 1);
        // 10 + 20.5 + 20.5 + 5 + 100; the text value is left out; the limit
        // on counts does not limit the stats.
        assert_eq!(stats, Some(FacetStats { min: 5.0, max: 100.0, avg: 156.0 / 5.0, sum: 156.0 }));
        let (_, stats) = f.counts("price", &[1, 2].into_iter().collect(), 10);
        assert_eq!(stats, Some(FacetStats { min: 20.5, max: 20.5, avg: 20.5, sum: 41.0 }));
        let (_, stats) = f.counts("price", &[4].into_iter().collect(), 10);
        assert_eq!(stats, None);
    }

    #[test]
    fn counts_are_sorted_and_limited() {
        let mut f = FacetIndex::new(&[FacetAttribute::try_from("c".to_owned()).unwrap()]);
        for (id, c) in ["b", "a", "b", "c", "c", "c"].iter().enumerate() {
            f.add(id as u32, &doc(json!({ "c": c })));
        }
        let (counts, _) = f.counts("c", &(0..6).collect(), 2);
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
                f.counts("tag", &candidates, usize::MAX).0.into_iter().map(|c| (c.value, c.count)).collect();
            assert_eq!(counted, by_value, "candidates {}", candidates.len());
        }
    }

    #[test]
    fn range_filters_agree_with_a_plain_scan() {
        let mut seed = 3u64;
        let mut next = |n: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let mut n = NumericIndex::default();
        let mut plain: Vec<(f64, u32)> = Vec::new();
        let pool = [-1e6, -250.5, -3.0, -0.5, 0.0, 0.25, 1.0, 1.5, 2.0, 2.01, 3.99, 4.0, 19.99, 20.0, 499.0, 1e9];
        let value = |k: u64| -> f64 {
            if k.is_multiple_of(2) { pool[(k / 2) as usize % pool.len()] } else { (k as f64 - 2000.0) / 7.0 }
        };
        for id in 0..2000u32 {
            let values: Vec<f64> = (0..1 + next(2)).map(|_| value(next(4000))).collect();
            n.add(id, &doc(json!({ "x": values })));
            plain.extend(values.iter().map(|&v| (v, id)));
        }
        // Remove some records, keeping the plain list in step.
        for id in (0..2000u32).step_by(5) {
            let values: Vec<f64> = plain.iter().filter(|(_, d)| *d == id).map(|(v, _)| *v).collect();
            n.remove(id, &doc(json!({ "x": values })));
            plain.retain(|(_, d)| *d != id);
        }
        for _ in 0..300 {
            let (a, b) = (value(next(4000)), value(next(4000)));
            let bound = |x: f64, kind: u64| match kind {
                0 => Included(x),
                1 => Excluded(x),
                _ => Unbounded,
            };
            let (low, high) = (bound(a.min(b), next(3)), bound(a.max(b), next(3)));
            let inside = |v: f64| {
                let above = match low { Included(x) => v >= x, Excluded(x) => v > x, Unbounded => true };
                let below = match high { Included(x) => v <= x, Excluded(x) => v < x, Unbounded => true };
                above && below
            };
            let expected: RoaringBitmap = plain.iter().filter(|(v, _)| inside(*v)).map(|(_, d)| *d).collect();
            assert_eq!(n.docs("x", low, high), expected, "{low:?} {high:?}");
        }
    }
}

