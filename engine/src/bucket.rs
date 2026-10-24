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

use crate::index::{CustomKey, Hit, Index, MAX_ORDERED_RULES, ordered};
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
    let mut groups: Vec<RoaringBitmap> = vec![candidates.clone()];

    for &criterion in &index.settings().ranking.0 {
        if groups.iter().all(|g| g.len() == 1) {
            break;
        }
        if signals.ties_everything(criterion) {
            continue;
        }
        let mut next = Vec::with_capacity(groups.len());
        let mut count = 0usize;
        for group in groups {
            if count >= needed {
                break;
            }
            let size = group.len() as usize;
            if size == 1 || count + size <= offset {
                count += size;
                next.push(group);
                continue;
            }
            let keep = needed - count;
            for subgroup in signals.split(criterion, group, keep) {
                count += subgroup.len() as usize;
                next.push(subgroup);
            }
        }
        groups = next;
    }

    // Inside a group every criterion ties, so records go in DocId order.
    let page: RoaringBitmap = groups.iter().flat_map(|g| g.iter()).skip(offset).take(len).collect();
    // Scoring only the page's records gives the same order as the full ranking.
    rank(index, terms, &page, boosts)
}

/// Keeps leading subgroups until they hold at least `keep` records; the
/// rest cannot reach the page.
fn first_needed(subgroups: impl IntoIterator<Item = RoaringBitmap>, keep: usize) -> Vec<RoaringBitmap> {
    let mut kept = Vec::new();
    let mut count = 0usize;
    for subgroup in subgroups {
        if count >= keep {
            break;
        }
        if !subgroup.is_empty() {
            count += subgroup.len() as usize;
            kept.push(subgroup);
        }
    }
    kept
}

/// Splits `group` by how many of `sets` contain each record. `ascending`
/// puts records in fewer sets first. Done with set operations only.
fn by_count(group: RoaringBitmap, sets: &[&RoaringBitmap], ascending: bool) -> Vec<RoaringBitmap> {
    // counts[c] = records of the group in exactly c of the sets seen so far.
    let mut counts: Vec<RoaringBitmap> = vec![group];
    for set in sets {
        let mut next = vec![RoaringBitmap::new(); counts.len() + 1];
        for (c, members) in counts.into_iter().enumerate() {
            let inside = &members & *set;
            next[c] |= &members - &inside;
            next[c + 1] |= inside;
        }
        counts = next;
    }
    if !ascending {
        counts.reverse();
    }
    counts
}

/// Per-record criterion values, computed on demand.
struct Signals<'a> {
    index: &'a Index,
    terms: &'a [QueryTerm],
    boosts: &'a [RoaringBitmap],
    /// For each term, the records whose best match has 0, 1 and 2 typos
    /// (disjoint sets).
    typo_levels: Vec<[RoaringBitmap; 3]>,
    /// For each term, the records matching it exactly.
    exact_sets: Vec<RoaringBitmap>,
    /// For each attribute, the records in which any term matches there.
    attribute_sets: Vec<RoaringBitmap>,
    /// Which words each term matches, for reading positions.
    term_words: Vec<TermWords>,
    /// Proximity of records gathered so far.
    proximity: HashMap<DocId, u32>,
}

impl<'a> Signals<'a> {
    fn new(index: &'a Index, terms: &'a [QueryTerm], boosts: &'a [RoaringBitmap]) -> Self {
        let mut typo_levels = Vec::with_capacity(terms.len());
        let mut exact_sets = Vec::with_capacity(terms.len());
        let mut attribute_sets: Vec<RoaringBitmap> = Vec::new();
        let mut add_attributes = |sets: &[RoaringBitmap]| {
            if attribute_sets.len() < sets.len() {
                attribute_sets.resize_with(sets.len(), RoaringBitmap::new);
            }
            for (into, docs) in attribute_sets.iter_mut().zip(sets) {
                *into |= docs;
            }
        };
        for term in terms {
            let mut by_typos: [RoaringBitmap; 3] = Default::default();
            let mut exact = RoaringBitmap::new();
            let no_typos = term.matches.iter().all(|m| m.typos == 0) && term.prefixes.iter().all(|p| p.typos == 0);
            if no_typos {
                by_typos[0] = term.docs.clone();
            }
            // Prefix groups use the index's precomputed prefix sets, so a
            // group of thousands of words costs one set.
            for p in &term.prefixes {
                if !no_typos && let Some(docs) = index.prefix_docs(&p.prefix) {
                    by_typos[usize::from(p.typos).min(2)] |= docs.as_ref();
                }
                add_attributes(&index.prefix_docs_by_attribute(&p.prefix));
            }
            for m in &term.matches {
                let Some(postings) = index.word(&m.word) else { continue };
                if !no_typos {
                    by_typos[usize::from(m.typos).min(2)] |= &postings.docs;
                }
                if m.typos == 0 && !m.prefix && !m.derived {
                    exact |= &postings.docs;
                }
                add_attributes(&postings.by_attribute);
            }
            // A record counts at its best (lowest) typo level only.
            let level0 = by_typos[0].clone();
            by_typos[1] -= &level0;
            let level1 = by_typos[1].clone();
            by_typos[2] -= &level0;
            by_typos[2] -= &level1;
            typo_levels.push(by_typos);
            exact_sets.push(exact);
        }
        let term_words = if terms.len() >= 2 { terms.iter().map(|t| TermWords::new(index, t)).collect() } else { Vec::new() };
        Signals { index, terms, boosts, typo_levels, exact_sets, attribute_sets, term_words, proximity: HashMap::new() }
    }

    /// Computes proximity for the records of `group` not seen yet. Needs at
    /// least two terms.
    fn gather(&mut self, group: &RoaringBitmap) {
        let n_terms = self.terms.len();
        let mut row: Vec<Vec<Hit>> = vec![Vec::new(); n_terms];
        for doc in group {
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

    /// True when `criterion` gives every candidate the same value, so it
    /// can be skipped without looking at records.
    fn ties_everything(&self, criterion: Criterion) -> bool {
        match criterion {
            Criterion::Typo => self.typo_levels.iter().all(|l| l[1].is_empty() && l[2].is_empty()),
            // Candidates match every required term.
            Criterion::Words => self.terms.iter().all(|t| !t.optional),
            Criterion::Filters => self.boosts.is_empty(),
            Criterion::Proximity => self.terms.len() < 2,
            Criterion::Attribute | Criterion::Exact => self.terms.is_empty(),
            Criterion::Custom => self.index.settings().custom_ranking.is_empty(),
        }
    }

    /// Splits `group` by `criterion` into ordered subgroups, keeping at
    /// least `keep` records and every record tied with the last kept.
    fn split(&mut self, criterion: Criterion, group: RoaringBitmap, keep: usize) -> Vec<RoaringBitmap> {
        match criterion {
            Criterion::Typo => {
                // totals[s] = records with s typos over the terms seen so far.
                let mut totals: Vec<RoaringBitmap> = vec![group];
                for levels in &self.typo_levels {
                    let mut next = vec![RoaringBitmap::new(); totals.len() + 2];
                    for (sum, members) in totals.into_iter().enumerate() {
                        let mut rest = members.clone();
                        for (k, level) in levels.iter().enumerate() {
                            let part = &members & level;
                            rest -= &part;
                            next[sum + k] |= part;
                        }
                        // Records the term does not match add no typos.
                        next[sum] |= rest;
                    }
                    totals = next;
                }
                first_needed(totals, keep)
            }
            Criterion::Words => {
                let sets: Vec<&RoaringBitmap> = self.terms.iter().map(|t| &t.docs).collect();
                first_needed(by_count(group, &sets, false), keep)
            }
            Criterion::Filters => {
                let sets: Vec<&RoaringBitmap> = self.boosts.iter().collect();
                first_needed(by_count(group, &sets, false), keep)
            }
            Criterion::Exact => {
                let sets: Vec<&RoaringBitmap> = self.exact_sets.iter().collect();
                first_needed(by_count(group, &sets, false), keep)
            }
            Criterion::Attribute => {
                let mut rest = group;
                let mut subgroups = Vec::with_capacity(self.attribute_sets.len() + 1);
                for set in &self.attribute_sets {
                    let part = &rest & set;
                    rest -= &part;
                    subgroups.push(part);
                }
                subgroups.push(rest);
                first_needed(subgroups, keep)
            }
            Criterion::Proximity => {
                self.gather(&group);
                let proximity = &self.proximity;
                to_bitmaps(by_key(group.iter().collect(), keep, |d| u64::from(proximity[&d])))
            }
            Criterion::Custom => {
                let index = self.index;
                let rules = index.settings().custom_ranking.len();
                // A large group: walk the index's sorted order and take its
                // members, instead of sorting the group.
                if let Some(order) = index.custom_order()
                    && group.len() * 8 >= index.all_docs_len()
                {
                    let mut subgroups: Vec<RoaringBitmap> = Vec::new();
                    let mut taken = 0usize;
                    let mut last: Option<&CustomKey> = None;
                    for (key, doc) in order {
                        if !group.contains(*doc) {
                            continue;
                        }
                        if last != Some(key) {
                            if taken >= keep {
                                break;
                            }
                            subgroups.push(RoaringBitmap::new());
                            last = Some(key);
                        }
                        subgroups.last_mut().expect("just pushed").insert(*doc);
                        taken += 1;
                    }
                    return subgroups;
                }
                let docs: Vec<DocId> = group.iter().collect();
                to_bitmaps(if rules <= MAX_ORDERED_RULES {
                    by_key(docs, keep, |d| index.custom_key(d))
                } else {
                    by_key(docs, keep, |d| custom_key_long(index, d))
                })
            }
        }
    }
}

/// The custom-ranking key of a record with more rules than a `CustomKey`
/// holds.
fn custom_key_long(index: &Index, doc: DocId) -> Vec<u64> {
    let rules = &index.settings().custom_ranking;
    (0..rules.len())
        .map(|i| match index.custom_value(i, doc) {
            Some(v) => {
                let key = ordered(v);
                if rules[i].descending { u64::MAX - 1 - key } else { key }
            }
            None => u64::MAX,
        })
        .collect()
}

fn to_bitmaps(groups: Vec<Vec<DocId>>) -> Vec<RoaringBitmap> {
    groups.into_iter().map(|g| g.into_iter().collect()).collect()
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
