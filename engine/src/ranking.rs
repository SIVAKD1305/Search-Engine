//! Tie-breaking ranking, Algolia's "ranking formula".
//!
//! There is no blended relevance score. Each criterion yields a small integer
//! per record, and records are sorted by the first criterion, then ties are
//! broken by the second, and so on. Default order:
//!
//! 1. `typo`      — fewer typos first
//! 2. `words`     — more query words matched first
//! 3. `proximity` — query words closer together first
//! 4. `attribute` — match in a more important attribute first
//! 5. `exact`     — more words matched exactly (whole word, no typo) first
//! 6. `custom`    — the index's own business metrics, e.g. `desc(rating)`

use crate::index::{AttributeId, Hit, Index};
use crate::search::QueryTerm;
use crate::store::DocId;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::cmp::Ordering;

/// Words further apart than this, or in different attributes, all count as
/// equally far. Algolia uses the same cap.
pub const MAX_PROXIMITY: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Criterion {
    Typo,
    Words,
    Proximity,
    Attribute,
    Exact,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Ranking(pub Vec<Criterion>);

impl Default for Ranking {
    fn default() -> Self {
        use Criterion::*;
        Ranking(vec![Typo, Words, Proximity, Attribute, Exact, Custom])
    }
}

/// One `customRanking` entry, written `desc(attribute)` or `asc(attribute)`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct CustomRule {
    pub attribute: String,
    pub descending: bool,
}

impl TryFrom<String> for CustomRule {
    type Error = String;

    fn try_from(rule: String) -> Result<Self, Self::Error> {
        let parse = |prefix: &str| {
            rule.strip_prefix(prefix)
                .and_then(|r| r.strip_suffix(')'))
                .filter(|a| !a.is_empty())
                .map(str::to_owned)
        };
        if let Some(attribute) = parse("desc(") {
            Ok(CustomRule { attribute, descending: true })
        } else if let Some(attribute) = parse("asc(") {
            Ok(CustomRule { attribute, descending: false })
        } else {
            Err(format!("custom ranking rule must be desc(attr) or asc(attr), got {rule:?}"))
        }
    }
}

impl From<CustomRule> for String {
    fn from(rule: CustomRule) -> String {
        let dir = if rule.descending { "desc" } else { "asc" };
        format!("{dir}({})", rule.attribute)
    }
}

/// Why a record ranked where it did (Algolia's `_rankingInfo`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RankingInfo {
    pub nb_typos: u32,
    pub words: u32,
    pub proximity_distance: u32,
    /// Most important attribute containing a query word.
    pub first_matched_attribute: Option<AttributeId>,
    pub nb_exact_words: u32,
    /// Values of the `customRanking` attributes, in rule order.
    pub custom: Vec<Option<f64>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankedDoc {
    pub doc: DocId,
    pub info: RankingInfo,
}

/// How one query term matched one record.
#[derive(Clone, Default)]
struct TermInDoc {
    /// Fewest typos among the matching words; `None` if the term did not match.
    typos: Option<u8>,
    exact: bool,
    hits: Vec<Hit>,
}

/// Sorts `candidates` by the index's ranking criteria.
pub fn rank(index: &Index, terms: &[QueryTerm], candidates: &RoaringBitmap) -> Vec<RankedDoc> {
    let docs: Vec<DocId> = candidates.iter().collect();
    let n_terms = terms.len();
    let mut cells: Vec<TermInDoc> = vec![TermInDoc::default(); docs.len() * n_terms];

    for (t, term) in terms.iter().enumerate() {
        for m in &term.matches {
            let Some(postings) = index.word(&m.word) else { continue };
            for doc in &postings.docs & candidates {
                let slot = docs.binary_search(&doc).expect("doc is a candidate");
                let cell = &mut cells[slot * n_terms + t];
                cell.typos = Some(cell.typos.map_or(m.typos, |t| t.min(m.typos)));
                cell.exact |= m.typos == 0 && !m.prefix;
                cell.hits.extend_from_slice(&postings.hits[&doc]);
            }
        }
    }

    let settings = index.settings();
    let mut ranked: Vec<RankedDoc> = docs
        .iter()
        .enumerate()
        .map(|(slot, &doc)| {
            let row = &mut cells[slot * n_terms..(slot + 1) * n_terms];
            for cell in row.iter_mut() {
                cell.hits.sort_unstable_by_key(|h| (h.attribute, h.position));
                cell.hits.dedup();
            }
            let record = index.store().get(doc).expect("candidate is live");
            RankedDoc { doc, info: ranking_info(row, record, &settings.custom_ranking) }
        })
        .collect();

    let criteria = &settings.ranking.0;
    ranked.sort_by(|a, b| {
        criteria
            .iter()
            .map(|c| compare(*c, &a.info, &b.info, &settings.custom_ranking))
            .find(|o| o.is_ne())
            .unwrap_or(Ordering::Equal)
            .then(a.doc.cmp(&b.doc))
    });
    ranked
}

fn ranking_info(row: &[TermInDoc], record: &Map<String, Value>, custom: &[CustomRule]) -> RankingInfo {
    let proximity_distance = row
        .windows(2)
        .map(|pair| pair_proximity(&pair[0].hits, &pair[1].hits))
        .sum();
    RankingInfo {
        nb_typos: row.iter().filter_map(|c| c.typos).map(u32::from).sum(),
        words: row.iter().filter(|c| c.typos.is_some()).count() as u32,
        proximity_distance,
        first_matched_attribute: row.iter().flat_map(|c| &c.hits).map(|h| h.attribute).min(),
        nb_exact_words: row.iter().filter(|c| c.exact).count() as u32,
        custom: custom.iter().map(|rule| record.get(&rule.attribute).and_then(Value::as_f64)).collect(),
    }
}

/// Distance between two consecutive query words in a record: the closest
/// pair of occurrences in the same attribute. Words in the wrong order cost
/// one more, so "wireless headphones" beats "headphones wireless" for the
/// query "wireless headphones".
fn pair_proximity(first: &[Hit], second: &[Hit]) -> u32 {
    let mut best = MAX_PROXIMITY;
    for a in first {
        for b in second {
            if a.attribute != b.attribute || a.position == b.position {
                continue;
            }
            let distance = if b.position > a.position {
                b.position - a.position
            } else {
                a.position - b.position + 1
            };
            best = best.min(distance);
            if best == 1 {
                return 1;
            }
        }
    }
    best
}

fn compare(criterion: Criterion, a: &RankingInfo, b: &RankingInfo, custom: &[CustomRule]) -> Ordering {
    match criterion {
        Criterion::Typo => a.nb_typos.cmp(&b.nb_typos),
        Criterion::Words => b.words.cmp(&a.words),
        Criterion::Proximity => a.proximity_distance.cmp(&b.proximity_distance),
        Criterion::Attribute => missing_last(a.first_matched_attribute, b.first_matched_attribute, |x, y| x.cmp(&y)),
        Criterion::Exact => b.nb_exact_words.cmp(&a.nb_exact_words),
        Criterion::Custom => custom
            .iter()
            .zip(a.custom.iter().zip(&b.custom))
            .map(|(rule, (x, y))| {
                missing_last(*x, *y, |x, y| {
                    let order = x.total_cmp(&y);
                    if rule.descending { order.reverse() } else { order }
                })
            })
            .find(|o| o.is_ne())
            .unwrap_or(Ordering::Equal),
    }
}

/// Orders present values with `cmp`; a missing value sorts after any present one.
fn missing_last<T>(x: Option<T>, y: Option<T>, cmp: impl FnOnce(T, T) -> Ordering) -> Ordering {
    match (x, y) {
        (Some(x), Some(y)) => cmp(x, y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Settings;
    use crate::search::search;
    use serde_json::json;

    fn index(settings: Settings, records: &[Value]) -> Index {
        let mut index = Index::new(settings);
        for r in records {
            index.add(r.clone()).unwrap();
        }
        index
    }

    fn products() -> Settings {
        serde_json::from_value(json!({
            "searchableAttributes": ["name", "description"],
            "customRanking": ["desc(rating)"]
        }))
        .unwrap()
    }

    fn order(index: &Index, query: &str) -> Vec<String> {
        search(index, query, 0, 100)
            .hits
            .iter()
            .map(|h| index.store().get(h.doc).unwrap()["objectID"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn settings_parse_algolia_names() {
        let s: Settings = serde_json::from_value(json!({
            "searchableAttributes": ["title"],
            "ranking": ["custom", "typo"],
            "customRanking": ["asc(price)", "desc(rating)"],
            "typoTolerance": false,
            "minWordSizefor1Typo": 5
        }))
        .unwrap();
        assert_eq!(s.ranking.0, [Criterion::Custom, Criterion::Typo]);
        assert_eq!(s.custom_ranking[0], CustomRule { attribute: "price".into(), descending: false });
        assert!(!s.typo.enabled);
        assert_eq!(s.typo.min_word_size_for_1_typo, 5);
        assert_eq!(s.typo.min_word_size_for_2_typos, 8);
        assert!(serde_json::from_value::<Settings>(json!({"customRanking": ["rating"]})).is_err());
    }

    #[test]
    fn fewer_typos_rank_first() {
        let i = index(products(), &[
            json!({"objectID": "typo", "name": "Kettel", "rating": 5}),
            json!({"objectID": "exact", "name": "Kettle", "rating": 1}),
        ]);
        assert_eq!(order(&i, "kettle"), ["exact", "typo"]);
    }

    #[test]
    fn closer_words_rank_first_and_order_matters() {
        let i = index(products(), &[
            json!({"objectID": "far", "name": "Wireless gaming mouse with headphones"}),
            json!({"objectID": "reversed", "name": "Headphones wireless"}),
            json!({"objectID": "adjacent", "name": "Wireless headphones"}),
        ]);
        assert_eq!(order(&i, "wireless headphones"), ["adjacent", "reversed", "far"]);
        let info = &search(&i, "wireless headphones", 0, 10).hits[0].info;
        assert_eq!(info.proximity_distance, 1);
    }

    #[test]
    fn words_in_different_attributes_count_as_far_apart() {
        let i = index(products(), &[
            json!({"objectID": "split", "name": "Wireless", "description": "headphones"}),
            json!({"objectID": "together", "name": "Speaker", "description": "wireless headphones"}),
        ]);
        assert_eq!(order(&i, "wireless headphones"), ["together", "split"]);
        let hits = search(&i, "wireless headphones", 0, 10).hits;
        assert_eq!(hits[1].info.proximity_distance, MAX_PROXIMITY);
    }

    #[test]
    fn earlier_attribute_ranks_first() {
        let i = index(products(), &[
            json!({"objectID": "in-description", "name": "Stand", "description": "for kettle", "rating": 5}),
            json!({"objectID": "in-name", "name": "Kettle", "description": "steel", "rating": 1}),
        ]);
        assert_eq!(order(&i, "kettle"), ["in-name", "in-description"]);
    }

    #[test]
    fn whole_words_beat_prefix_matches() {
        let i = index(products(), &[
            json!({"objectID": "prefix", "name": "Headphones", "rating": 5}),
            json!({"objectID": "whole", "name": "Head Massager", "rating": 1}),
        ]);
        assert_eq!(order(&i, "head"), ["whole", "prefix"]);
    }

    #[test]
    fn custom_ranking_breaks_remaining_ties_and_missing_values_sort_last() {
        let i = index(products(), &[
            json!({"objectID": "low", "name": "Kettle", "rating": 2.5}),
            json!({"objectID": "none", "name": "Kettle"}),
            json!({"objectID": "high", "name": "Kettle", "rating": 4.8}),
        ]);
        assert_eq!(order(&i, "kettle"), ["high", "low", "none"]);
        // The empty query returns everything, ordered by custom ranking only.
        assert_eq!(order(&i, ""), ["high", "low", "none"]);
    }

    #[test]
    fn criteria_order_is_configurable() {
        let mut settings = products();
        settings.ranking = Ranking(vec![Criterion::Custom, Criterion::Typo]);
        let i = index(settings, &[
            json!({"objectID": "exact", "name": "Kettle", "rating": 1}),
            json!({"objectID": "typo", "name": "Kettel", "rating": 5}),
        ]);
        assert_eq!(order(&i, "kettle"), ["typo", "exact"]);
    }

    #[test]
    fn pages_slice_the_ranked_list() {
        let records: Vec<Value> =
            (0..25).map(|n| json!({"objectID": n.to_string(), "name": "Kettle", "rating": n})).collect();
        let i = index(products(), &records);
        let page = search(&i, "kettle", 2, 10);
        assert_eq!((page.nb_hits, page.nb_pages, page.hits.len()), (25, 3, 5));
        let ids: Vec<&str> =
            page.hits.iter().map(|h| i.store().get(h.doc).unwrap()["objectID"].as_str().unwrap()).collect();
        assert_eq!(ids, ["4", "3", "2", "1", "0"]);
        assert!(search(&i, "kettle", 9, 10).hits.is_empty());
    }

    #[test]
    fn pair_proximity_examples() {
        let h = |attribute, position| Hit { attribute, position };
        assert_eq!(pair_proximity(&[h(0, 0)], &[h(0, 1)]), 1);
        assert_eq!(pair_proximity(&[h(0, 3)], &[h(0, 1)]), 3);
        assert_eq!(pair_proximity(&[h(0, 0)], &[h(1, 1)]), MAX_PROXIMITY);
        assert_eq!(pair_proximity(&[h(0, 0)], &[h(0, 50)]), MAX_PROXIMITY);
        assert_eq!(pair_proximity(&[h(0, 0), h(0, 10)], &[h(0, 4), h(0, 12)]), 2);
    }
}
