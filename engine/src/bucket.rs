//! Staged ranking ("bucket sort"), the way Algolia avoids scoring every hit.
//!
//! Records are kept in ordered groups. Each criterion, in ranking order,
//! splits only the groups that can still reach the requested page; groups
//! entirely before the page are never needed in order, and groups after it
//! are dropped. When a group is larger than the number of records still
//! needed, only its best records are kept, plus every record tied with the
//! last one kept (the next criterion may reorder those).
//!
//! Most criteria (typo, words, filters, attribute, exact, custom) come from
//! bitmaps and columns. Word positions, needed only for proximity, are
//! gathered only for records in groups that still compete for the page.
//!
//! The records chosen for the page are then scored by `ranking::rank`, so
//! the order and `_rankingInfo` are exactly those of the full ranking.

use crate::index::{Hit, Index};
use crate::ranking::{Criterion, RankedDoc, TermWords, pair_proximity, rank};
use crate::search::QueryTerm;
use crate::store::DocId;
use roaring::RoaringBitmap;
use std::collections::HashMap;

/// Ranks `candidates` and returns positions `offset..offset + len` of the
/// ranked list, with ranking information.
pub fn rank_page(
    index: &Index,
    terms: &[QueryTerm],
    candidates: &RoaringBitmap,
    boosts: &[RoaringBitmap],
    offset: usize,
    len: usize,
) -> Vec<RankedDoc> {
    let needed = offset.saturating_add(len);
    if len == 0 || offset >= candidates.len() as usize {
        return Vec::new();
    }
    let mut signals = Signals::new(index, terms, boosts);
    let mut groups: Vec<Vec<DocId>> = vec![candidates.iter().collect()];

    for &criterion in &index.settings().ranking.0 {
        if groups.iter().all(|g| g.len() == 1) {
            break;
        }
        let mut next = Vec::with_capacity(groups.len());
        let mut count = 0usize;
        for group in groups {
            if count >= needed {
                break;
            }
            let size = group.len();
            if size == 1 || count + size <= offset {
                count += size;
                next.push(group);
                continue;
            }
            let keep = (needed - count).min(size);
            let subgroups = signals.split(criterion, group, keep);
            count += subgroups.iter().map(Vec::len).sum::<usize>();
            next.extend(subgroups);
        }
        groups = next;
    }

    let page: RoaringBitmap = groups.into_iter().flatten().skip(offset).take(len).collect();
    // Scoring only the page's records gives the same order as the full ranking.
    rank(index, terms, &page, boosts)
}

/// Per-record criterion values, computed on demand.
struct Signals<'a> {
    index: &'a Index,
    terms: &'a [QueryTerm],
    boosts: &'a [RoaringBitmap],
    /// For each term, the records matching it with 0, 1 and 2 typos.
    typo_sets: Vec<Vec<RoaringBitmap>>,
    /// For each term, the records matching it exactly.
    exact_sets: Vec<RoaringBitmap>,
    /// For each term, the records matching it in each attribute.
    attribute_sets: Vec<Vec<RoaringBitmap>>,
    /// Which words each term matches, for reading positions.
    term_words: Vec<TermWords>,
    /// Proximity of records gathered so far.
    proximity: HashMap<DocId, u32>,
}

impl<'a> Signals<'a> {
    fn new(index: &'a Index, terms: &'a [QueryTerm], boosts: &'a [RoaringBitmap]) -> Self {
        let mut typo_sets = Vec::with_capacity(terms.len());
        let mut exact_sets = Vec::with_capacity(terms.len());
        let mut attribute_sets = Vec::with_capacity(terms.len());
        for term in terms {
            let mut by_typos = vec![RoaringBitmap::new(); 3];
            let mut exact = RoaringBitmap::new();
            let mut by_attribute: Vec<RoaringBitmap> = Vec::new();
            // A prefix matched without typos covers every word starting with
            // it: use the precomputed prefix sets instead of thousands of unions.
            let whole_prefix = term.is_prefix && term.matches.iter().all(|m| m.typos == 0);
            if term.matches.iter().all(|m| m.typos == 0) {
                by_typos[0] = term.docs.clone();
            }
            if whole_prefix {
                by_attribute = index.prefix_docs_by_attribute(&term.text).to_vec();
            }
            for m in &term.matches {
                let Some(postings) = index.word(&m.word) else { continue };
                if m.typos > 0 {
                    by_typos[usize::from(m.typos).min(2)] |= &postings.docs;
                } else if by_typos[0].is_empty() || m.derived {
                    by_typos[0] |= &postings.docs;
                }
                if m.typos == 0 && !m.prefix && !m.derived {
                    exact |= &postings.docs;
                }
                if !whole_prefix || m.derived {
                    if by_attribute.len() < postings.by_attribute.len() {
                        by_attribute.resize_with(postings.by_attribute.len(), RoaringBitmap::new);
                    }
                    for (set, docs) in by_attribute.iter_mut().zip(&postings.by_attribute) {
                        *set |= docs;
                    }
                }
            }
            typo_sets.push(by_typos);
            exact_sets.push(exact);
            attribute_sets.push(by_attribute);
        }
        let term_words = if terms.len() >= 2 { terms.iter().map(|t| TermWords::new(index, t)).collect() } else { Vec::new() };
        Signals { index, terms, boosts, typo_sets, exact_sets, attribute_sets, term_words, proximity: HashMap::new() }
    }

    fn typos(&self, doc: DocId) -> u64 {
        self.typo_sets
            .iter()
            .filter_map(|levels| levels.iter().position(|set| set.contains(doc)))
            .map(|t| t as u64)
            .sum()
    }

    /// The most important attribute in which any term matches.
    fn attribute(&self, doc: DocId) -> u64 {
        self.attribute_sets
            .iter()
            .filter_map(|sets| sets.iter().position(|set| set.contains(doc)))
            .min()
            .map_or(u64::MAX, |a| a as u64)
    }

    fn count(sets: impl Iterator<Item = bool>) -> u64 {
        sets.filter(|&b| b).count() as u64
    }

    /// Computes proximity for the records of `group` not seen yet. Needs at
    /// least two terms.
    fn gather(&mut self, group: &[DocId]) {
        let n_terms = self.terms.len();
        let mut row: Vec<Vec<Hit>> = vec![Vec::new(); n_terms];
        for &doc in group {
            if self.proximity.contains_key(&doc) {
                continue;
            }
            for hits in &mut row {
                hits.clear();
            }
            // Occurrences are in reading order, so each term's hits come out sorted.
            for occurrence in self.index.occurrences(doc) {
                for (t, words) in self.term_words.iter().enumerate() {
                    if words.get(occurrence.word).is_some() {
                        row[t].push(occurrence.hit());
                    }
                }
            }
            let proximity = row.windows(2).map(|pair| pair_proximity(&pair[0], &pair[1])).sum();
            self.proximity.insert(doc, proximity);
        }
    }

    /// Splits `group` by `criterion` into ordered subgroups, keeping at
    /// least `keep` records and every record tied with the last kept.
    /// True when `criterion` gives every candidate the same value, so it
    /// can be skipped without looking at records.
    fn ties_everything(&self, criterion: Criterion) -> bool {
        match criterion {
            Criterion::Typo => self.typo_sets.iter().all(|levels| levels[1..].iter().all(RoaringBitmap::is_empty)),
            // Candidates match every required term.
            Criterion::Words => self.terms.iter().all(|t| !t.optional),
            Criterion::Filters => self.boosts.is_empty(),
            Criterion::Proximity => self.terms.len() < 2,
            Criterion::Attribute | Criterion::Exact => self.terms.is_empty(),
            Criterion::Custom => self.index.settings().custom_ranking.is_empty(),
        }
    }

    fn split(&mut self, criterion: Criterion, group: Vec<DocId>, keep: usize) -> Vec<Vec<DocId>> {
        if self.ties_everything(criterion) {
            return vec![group];
        }
        match criterion {
            Criterion::Typo => by_key(group, keep, |d| self.typos(d)),
            Criterion::Words => by_key(group, keep, |d| u64::MAX - Self::count(self.terms.iter().map(|t| t.docs.contains(d)))),
            Criterion::Filters => by_key(group, keep, |d| u64::MAX - Self::count(self.boosts.iter().map(|b| b.contains(d)))),
            Criterion::Exact => {
                by_key(group, keep, |d| u64::MAX - Self::count(self.exact_sets.iter().map(|s| s.contains(d))))
            }
            Criterion::Attribute => by_key(group, keep, |d| self.attribute(d)),
            Criterion::Proximity => {
                self.gather(&group);
                let proximity = &self.proximity;
                by_key(group, keep, |d| u64::from(proximity[&d]))
            }
            Criterion::Custom => {
                let rules = &self.index.settings().custom_ranking;
                let value = |i: usize, d: DocId| match self.index.custom_value(i, d) {
                    Some(v) => {
                        let key = ordered(v);
                        // Missing values (u64::MAX) sort last either way.
                        if rules[i].descending { u64::MAX - 1 - key } else { key }
                    }
                    None => u64::MAX,
                };
                if rules.len() <= 4 {
                    // A fixed-size key avoids an allocation per record.
                    by_key(group, keep, |d| std::array::from_fn::<u64, 4, _>(|i| if i < rules.len() { value(i, d) } else { 0 }))
                } else {
                    by_key(group, keep, |d| (0..rules.len()).map(|i| value(i, d)).collect::<Vec<u64>>())
                }
            }
        }
    }
}

/// Maps an f64 to a u64 with the same order (below u64::MAX).
fn ordered(v: f64) -> u64 {
    let bits = (v + 0.0).to_bits();
    let key = if bits >> 63 == 1 { !bits } else { bits | (1 << 63) };
    key.min(u64::MAX - 2)
}

/// Orders `group` by `key` (smaller first, ties by DocId) and splits it into
/// runs of equal keys. Records ranked after the `keep`-th one and not tied
/// with it are dropped.
fn by_key<K: Ord + Clone>(group: Vec<DocId>, keep: usize, mut key: impl FnMut(DocId) -> K) -> Vec<Vec<DocId>> {
    let mut pairs: Vec<(K, DocId)> = group.into_iter().map(|d| (key(d), d)).collect();
    // Common for large groups: the criterion ties them all, nothing to order.
    if pairs.windows(2).all(|w| w[0].0 == w[1].0) {
        return vec![pairs.into_iter().map(|(_, d)| d).collect()];
    }
    if keep > 0 && keep < pairs.len() {
        pairs.select_nth_unstable_by(keep - 1, |a, b| a.0.cmp(&b.0));
        let boundary = pairs[keep - 1].0.clone();
        pairs.retain(|(k, _)| *k <= boundary);
    }
    pairs.sort_unstable();
    let mut groups: Vec<Vec<DocId>> = Vec::new();
    let mut last: Option<K> = None;
    for (k, d) in pairs {
        if last.as_ref() != Some(&k) {
            groups.push(Vec::new());
            last = Some(k);
        }
        groups.last_mut().expect("just pushed").push(d);
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Settings;
    use crate::search::{QueryOptions, matching, plan_with};
    use serde_json::json;

    #[test]
    fn ordered_keeps_float_order() {
        let values = [f64::NEG_INFINITY, -2.5, -0.0, 0.0, 1e-9, 3.0, 1e300];
        for w in values.windows(2) {
            assert!(ordered(w[0]) <= ordered(w[1]), "{} vs {}", w[0], w[1]);
        }
        assert_eq!(ordered(-0.0), ordered(0.0));
        assert!(ordered(f64::INFINITY) < u64::MAX - 1);
    }

    #[test]
    fn by_key_keeps_ties_at_the_cut() {
        let groups = by_key(vec![1, 2, 3, 4, 5, 6], 2, |d| [3, 1, 1, 2, 2, 9][d as usize - 1]);
        // Two needed: records 2 and 3 (key 1); keys 2 and above are cut.
        assert_eq!(groups, [vec![2, 3]]);
        let groups = by_key(vec![1, 2, 3, 4, 5, 6], 3, |d| [3, 1, 1, 2, 2, 9][d as usize - 1]);
        // Three needed: the third (key 2) is tied with record 5, so both stay.
        assert_eq!(groups, [vec![2, 3], vec![4, 5]]);
    }

    /// A small deterministic random generator, to avoid a test dependency.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) % n as u64) as usize
        }
    }

    fn random_index(rng: &mut Lcg, settings: Settings) -> Index {
        let words = ["wireless", "headphones", "kettle", "black", "lamp", "desk", "phone", "case", "head", "set"];
        let mut index = Index::new(settings);
        for id in 0..400 {
            let name: Vec<&str> = (0..1 + rng.next(4)).map(|_| words[rng.next(words.len())]).collect();
            let description: Vec<&str> = (0..rng.next(6)).map(|_| words[rng.next(words.len())]).collect();
            let brand = ["Orbix", "Elan", "Tundra"][rng.next(3)];
            let mut record = json!({
                "objectID": id.to_string(),
                "name": name.join(" "),
                "description": description.join(" "),
                "brand": brand,
            });
            // Some records lack the custom ranking attribute, some tie.
            if rng.next(5) > 0 {
                record["rating"] = json!(rng.next(6) as f64 / 2.0);
            }
            if rng.next(2) == 0 {
                record["sales"] = json!(rng.next(3));
            }
            index.add(record).unwrap();
        }
        index
    }

    #[test]
    fn pages_match_the_full_ranking() {
        let mut rng = Lcg(7);
        let queries = [
            "", "w", "head", "wireless head", "wirless headphnes", "kettle black", "black kettle", "lamp desk phone",
            "set", "case phone", "headphones wireless", "k", "desk lamp", "zzz",
        ];
        for ranking in [
            json!(["typo", "words", "filters", "proximity", "attribute", "exact", "custom"]),
            json!(["custom", "typo", "proximity"]),
            json!(["exact", "attribute", "proximity", "words", "typo"]),
        ] {
            let settings: Settings = serde_json::from_value(json!({
                "searchableAttributes": ["name", "description"],
                "attributesForFaceting": ["brand"],
                "customRanking": ["desc(rating)", "asc(sales)"],
                "ranking": ranking,
            }))
            .unwrap();
            let index = random_index(&mut rng, settings);
            for query in queries {
                for all_optional in [false, true] {
                    let mut terms = plan_with(&index, query, &QueryOptions::from_settings(index.settings()));
                    for t in &mut terms {
                        t.optional = all_optional;
                    }
                    let candidates = matching(&index, &terms);
                    for boosts in [vec![], vec![index.facet_docs("brand", "Orbix")]] {
                        let full = rank(&index, &terms, &candidates, &boosts);
                        for (offset, len) in [(0, 1), (0, 5), (0, 20), (3, 7), (20, 20), (390, 20), (0, 1000), (1000, 5)] {
                            let page = rank_page(&index, &terms, &candidates, &boosts, offset, len);
                            let expected: Vec<&RankedDoc> = full.iter().skip(offset).take(len).collect();
                            assert_eq!(
                                page.iter().collect::<Vec<_>>(),
                                expected,
                                "query {query:?} optional {all_optional} boosts {} offset {offset} len {len} ranking {:?}",
                                boosts.len(),
                                index.settings().ranking
                            );
                        }
                    }
                }
            }
        }
    }
}
