//! Query execution: matching with typo tolerance, filters, facets and ranking.
//!
//! Each query word is expanded into the dictionary words it can stand for:
//! itself, words within the allowed number of typos, and — for the last word,
//! which the user may still be typing — words that start with it (Algolia's
//! default `queryType: prefixLast`). A record matches when every query word
//! matches at least one of its expansions.

use crate::facets::FacetCount;
use crate::filter::{self, FilterError};
use crate::highlight::{self, HighlightTags, Matcher};
use crate::index::Index;
use crate::ranking::{RankingInfo, rank};
use crate::store::DocId;
use crate::tokenizer::tokenize;
use crate::typo::{WordMatch, fuzzy_matches};
use roaring::RoaringBitmap;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct QueryTerm {
    /// Normalized query word.
    pub text: String,
    pub is_prefix: bool,
    /// Dictionary words this term can match, fewest typos first.
    pub matches: Vec<WordMatch>,
    /// Records containing at least one of `matches`.
    pub docs: RoaringBitmap,
}

/// Expands every query word into its matching dictionary words.
pub fn plan(index: &Index, query: &str) -> Vec<QueryTerm> {
    let tokens = tokenize(query);
    let last = tokens.len().saturating_sub(1);
    tokens
        .into_iter()
        .enumerate()
        .map(|(i, token)| {
            let is_prefix = i == last;
            let max_typos = index.settings().typo.max_typos(token.text.chars().count());
            let mut matches = fuzzy_matches(index.dictionary(), &token.text, max_typos, is_prefix);
            matches.sort_by(|a, b| a.typos.cmp(&b.typos).then_with(|| a.word.cmp(&b.word)));
            let docs = term_docs(index, &token.text, is_prefix, max_typos, &matches);
            QueryTerm { text: token.text, is_prefix, matches, docs }
        })
        .collect()
}

fn term_docs(index: &Index, text: &str, is_prefix: bool, max_typos: u8, matches: &[WordMatch]) -> RoaringBitmap {
    // Exact prefix: one precomputed set instead of a union over every word.
    if is_prefix && max_typos == 0 {
        return index.prefix_docs(text).cloned().unwrap_or_default();
    }
    let mut docs = RoaringBitmap::new();
    for m in matches {
        if let Some(postings) = index.word(&m.word) {
            docs |= &postings.docs;
        }
    }
    docs
}

/// A search request, with Algolia's parameter names when read from JSON.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SearchParams {
    pub query: String,
    /// Required conditions, in Algolia's `filters` syntax.
    pub filters: Option<String>,
    /// Conditions that boost records matching them, without excluding others.
    pub optional_filters: Vec<String>,
    /// Facet attributes to count; `"*"` means all of them.
    pub facets: Vec<String>,
    pub page: usize,
    pub hits_per_page: usize,
    pub max_values_per_facet: usize,
    /// Attributes to highlight; `None` (or `["*"]`) means all searchable ones.
    pub attributes_to_highlight: Option<Vec<String>>,
    /// Attributes to snippet, as `attribute` or `attribute:words`.
    pub attributes_to_snippet: Vec<String>,
    pub highlight_pre_tag: String,
    pub highlight_post_tag: String,
    pub snippet_ellipsis_text: String,
}

impl Default for SearchParams {
    fn default() -> Self {
        SearchParams {
            query: String::new(),
            filters: None,
            optional_filters: Vec::new(),
            facets: Vec::new(),
            page: 0,
            hits_per_page: 20,
            max_values_per_facet: 100,
            attributes_to_highlight: None,
            attributes_to_snippet: Vec::new(),
            highlight_pre_tag: HighlightTags::default().pre,
            highlight_post_tag: HighlightTags::default().post,
            snippet_ellipsis_text: HighlightTags::default().ellipsis,
        }
    }
}

impl SearchParams {
    pub fn new(query: impl Into<String>) -> Self {
        SearchParams { query: query.into(), ..Self::default() }
    }
}

#[derive(Debug)]
pub struct SearchHit {
    pub doc: DocId,
    pub info: RankingInfo,
    /// Algolia's `_highlightResult`.
    pub highlight: Map<String, Value>,
    /// Algolia's `_snippetResult`.
    pub snippet: Map<String, Value>,
}

#[derive(Debug)]
pub struct SearchResults {
    /// The requested page of ranked records.
    pub hits: Vec<SearchHit>,
    pub nb_hits: u64,
    pub page: usize,
    pub nb_pages: usize,
    pub hits_per_page: usize,
    /// Value counts per requested facet, over all hits (not just this page).
    pub facets: BTreeMap<String, Vec<FacetCount>>,
    pub terms: Vec<QueryTerm>,
}

/// Runs a query and returns one page (0-based) of ranked results.
pub fn search(index: &Index, params: &SearchParams) -> Result<SearchResults, FilterError> {
    let hits_per_page = params.hits_per_page.max(1);
    let terms = plan(index, &params.query);
    let mut candidates = intersect(index, &terms);
    if let Some(filters) = params.filters.as_deref().filter(|f| !f.trim().is_empty()) {
        candidates &= filter::parse(filters)?.evaluate(index)?;
    }
    let boosts = params
        .optional_filters
        .iter()
        .map(|f| filter::parse(f)?.evaluate(index))
        .collect::<Result<Vec<_>, _>>()?;

    let mut facets = BTreeMap::new();
    for requested in &params.facets {
        let attributes: Vec<&str> =
            if requested == "*" { index.countable_facets().collect() } else { vec![requested.as_str()] };
        for attribute in attributes {
            if index.countable_facets().any(|a| a == attribute) {
                facets.insert(
                    attribute.to_owned(),
                    index.facet_counts(attribute, &candidates, params.max_values_per_facet),
                );
            }
        }
    }

    let ranked = rank(index, &terms, &candidates, &boosts);
    let nb_hits = candidates.len();
    let page = ranked.into_iter().skip(params.page.saturating_mul(hits_per_page)).take(hits_per_page);

    let matcher = Matcher::new(&terms);
    let tags = HighlightTags {
        pre: params.highlight_pre_tag.clone(),
        post: params.highlight_post_tag.clone(),
        ellipsis: params.snippet_ellipsis_text.clone(),
    };
    let to_highlight: Vec<String> = match &params.attributes_to_highlight {
        Some(list) if list.iter().any(|a| a == "*") => index.attributes().to_vec(),
        Some(list) => list.clone(),
        None => index.attributes().to_vec(),
    };
    let hits = page
        .map(|r| {
            let record = index.store().get(r.doc).expect("ranked record is live");
            SearchHit {
                doc: r.doc,
                info: r.info,
                highlight: highlight::highlight(record, &to_highlight, &matcher, &tags),
                snippet: highlight::snippet(record, &params.attributes_to_snippet, &matcher, &tags),
            }
        })
        .collect();
    Ok(SearchResults {
        hits,
        nb_hits,
        page: params.page,
        nb_pages: (nb_hits as usize).div_ceil(hits_per_page),
        hits_per_page,
        facets,
        terms,
    })
}

/// Records matching every word of `query`. An empty query matches everything.
pub fn candidates(index: &Index, query: &str) -> RoaringBitmap {
    intersect(index, &plan(index, query))
}

pub fn intersect(index: &Index, terms: &[QueryTerm]) -> RoaringBitmap {
    if terms.is_empty() {
        return index.all_docs();
    }
    // Intersect the smallest sets first so the working set shrinks quickly.
    let mut sets: Vec<&RoaringBitmap> = terms.iter().map(|t| &t.docs).collect();
    sets.sort_unstable_by_key(|docs| docs.len());
    let mut result = sets[0].clone();
    for docs in &sets[1..] {
        if result.is_empty() {
            break;
        }
        result &= *docs;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Settings;
    use crate::typo::TypoSettings;
    use serde_json::json;

    fn index_with(settings: Settings) -> Index {
        let mut index = Index::new(settings);
        for (id, name) in [
            ("1", "Wireless Headphones"),
            ("2", "Wireless Mouse"),
            ("3", "Headphone Stand"),
            ("4", "Élan Kettle"),
            ("5", "Mickael Rossi"),
        ] {
            index.add(json!({"objectID": id, "name": name})).unwrap();
        }
        index
    }

    fn ids(index: &Index, query: &str) -> Vec<String> {
        candidates(index, query)
            .iter()
            .map(|id| index.store().get(id).unwrap()["objectID"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn last_word_is_a_prefix_and_other_words_are_whole() {
        let index = index_with(Settings::default());
        assert_eq!(ids(&index, "wireless head"), ["1"]);
        assert_eq!(ids(&index, "head"), ["1", "3"]);
        // "head" is too short for typos and is not a whole word.
        assert!(ids(&index, "head wireless").is_empty());
    }

    #[test]
    fn queries_are_normalized_like_records() {
        let index = index_with(Settings::default());
        assert_eq!(ids(&index, "ELAN"), ["4"]);
        assert_eq!(ids(&index, "élan ket"), ["4"]);
    }

    #[test]
    fn typos_are_tolerated_in_whole_and_prefix_words() {
        let index = index_with(Settings::default());
        assert_eq!(ids(&index, "wirless mouse"), ["2"]);
        assert_eq!(ids(&index, "wireles hedphones"), ["1"]);
        assert_eq!(ids(&index, "mikc"), ["5"]);
        // As a whole word, "headphone" also matches "headphones" with one typo.
        assert_eq!(ids(&index, "headphone stand"), ["3"]);
        assert_eq!(ids(&index, "wireless headphone"), ["1"]);
    }

    #[test]
    fn typo_matches_are_listed_fewest_typos_first() {
        let index = index_with(Settings::default());
        let terms = plan(&index, "headphone ");
        let words: Vec<(&str, u8)> = terms[0].matches.iter().map(|m| (m.word.as_str(), m.typos)).collect();
        assert_eq!(words, [("headphone", 0), ("headphones", 0)]);
        let terms = plan(&index, "headphone stand");
        let words: Vec<(&str, u8)> = terms[0].matches.iter().map(|m| (m.word.as_str(), m.typos)).collect();
        assert_eq!(words, [("headphone", 0), ("headphones", 1)]);
    }

    #[test]
    fn typo_tolerance_can_be_disabled() {
        let index = index_with(Settings {
            typo: TypoSettings { enabled: false, ..TypoSettings::default() },
            ..Settings::default()
        });
        assert!(ids(&index, "wirless mouse").is_empty());
        assert_eq!(ids(&index, "wireless mouse"), ["2"]);
    }

    #[test]
    fn unknown_words_match_nothing_and_empty_query_matches_all() {
        let index = index_with(Settings::default());
        assert!(ids(&index, "wireless toaster").is_empty());
        assert_eq!(ids(&index, "").len(), 5);
        assert_eq!(ids(&index, " ,, ").len(), 5);
    }

    fn shop() -> Index {
        let settings: Settings = serde_json::from_value(json!({
            "searchableAttributes": ["name"],
            "attributesForFaceting": ["brand", "color", "in_stock", "filterOnly(sku)"],
            "customRanking": ["desc(rating)"]
        }))
        .unwrap();
        let mut index = Index::new(settings);
        for r in [
            json!({"objectID": "1", "name": "Kettle", "brand": "Élan", "color": "Black", "price": 25, "rating": 4.0, "in_stock": true, "sku": "K-1"}),
            json!({"objectID": "2", "name": "Kettle", "brand": "Orbix", "color": "White", "price": 80, "rating": 4.5, "in_stock": false, "sku": "K-2"}),
            json!({"objectID": "3", "name": "Kettle", "brand": "Élan", "color": "White", "price": 120, "rating": 3.0, "in_stock": true, "sku": "K-3"}),
            json!({"objectID": "4", "name": "Toaster", "brand": "Élan", "color": "Black", "price": 40, "rating": 5.0, "in_stock": true, "sku": "T-1"}),
        ] {
            index.add(r).unwrap();
        }
        index
    }

    fn found(index: &Index, query: &str, filters: &str) -> Vec<String> {
        let params = SearchParams { filters: Some(filters.into()), ..SearchParams::new(query) };
        search(index, &params)
            .unwrap()
            .hits
            .iter()
            .map(|h| index.store().get(h.doc).unwrap()["objectID"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn filters_combine_facets_numbers_and_logic() {
        let index = shop();
        assert_eq!(found(&index, "kettle", "brand:elan"), ["1", "3"]);
        assert_eq!(found(&index, "kettle", "brand:Élan AND price < 100"), ["1"]);
        assert_eq!(found(&index, "", "price:40 TO 120 AND NOT color:white"), ["4"]);
        assert_eq!(found(&index, "kettle", "(color:Black OR brand:Orbix) AND in_stock:true"), ["1"]);
        assert_eq!(found(&index, "", "price != 25 AND price >= 80"), ["2", "3"]);
        assert_eq!(found(&index, "", "sku:K-2"), ["2"]);
        // A non-facet numeric attribute can be matched with ':' too.
        assert_eq!(found(&index, "", "price:80"), ["2"]);
        assert_eq!(found(&index, "kettle", "  "), ["2", "1", "3"]);
    }

    #[test]
    fn invalid_filters_are_errors() {
        let index = shop();
        let error = |f: &str| {
            search(&index, &SearchParams { filters: Some(f.into()), ..SearchParams::new("") }).unwrap_err().0
        };
        assert_eq!(error("name:Kettle"), "'name' is not in attributesForFaceting");
        assert_eq!(error("price <"), "missing value for 'price'");
    }

    #[test]
    fn facet_counts_cover_all_filtered_hits() {
        let index = shop();
        let params = SearchParams {
            facets: vec!["*".into()],
            filters: Some("in_stock:true".into()),
            hits_per_page: 1,
            ..SearchParams::new("")
        };
        let results = search(&index, &params).unwrap();
        assert_eq!(results.hits.len(), 1);
        let counts = |attr: &str| -> Vec<(String, u64)> {
            results.facets[attr].iter().map(|c| (c.value.clone(), c.count)).collect()
        };
        assert_eq!(counts("brand"), [("Élan".to_owned(), 3)]);
        assert_eq!(counts("color"), [("Black".to_owned(), 2), ("White".to_owned(), 1)]);
        // filterOnly attributes never return counts.
        assert!(!results.facets.contains_key("sku"));
        assert_eq!(results.facets.keys().collect::<Vec<_>>(), ["brand", "color", "in_stock"]);
    }

    #[test]
    fn optional_filters_boost_without_excluding() {
        let index = shop();
        let params = SearchParams { optional_filters: vec!["brand:Orbix".into()], ..SearchParams::new("kettle") };
        let results = search(&index, &params).unwrap();
        let order: Vec<&str> =
            results.hits.iter().map(|h| index.store().get(h.doc).unwrap()["objectID"].as_str().unwrap()).collect();
        assert_eq!(order, ["2", "1", "3"]);
        assert_eq!(results.hits[0].info.filters, 1);
        assert_eq!(results.nb_hits, 3);
    }

    #[test]
    fn deleted_and_replaced_records_leave_filters() {
        let mut index = shop();
        index.delete("4");
        assert!(found(&index, "", "price:40").is_empty());
        index.add(json!({"objectID": "1", "name": "Kettle", "brand": "Orbix", "price": 30})).unwrap();
        assert_eq!(found(&index, "", "brand:elan"), ["3"]);
        assert_eq!(found(&index, "", "price < 50"), ["1"]);
    }

    #[test]
    fn search_params_read_algolia_json() {
        let params: SearchParams = serde_json::from_value(json!({
            "query": "kettle", "filters": "price < 50", "hitsPerPage": 5, "facets": ["brand"]
        }))
        .unwrap();
        assert_eq!((params.hits_per_page, params.page, params.max_values_per_facet), (5, 0, 100));
        assert_eq!(params.filters.as_deref(), Some("price < 50"));
    }
}

#[cfg(test)]
mod highlight_tests {
    use super::*;
    use crate::index::Settings;
    use serde_json::json;

    #[test]
    fn hits_carry_highlights_and_snippets() {
        let settings: Settings =
            serde_json::from_value(json!({"searchableAttributes": ["title", "body"]})).unwrap();
        let mut index = Index::new(settings);
        index
            .add(json!({"objectID": "1", "title": "Raft explained", "body": "Leaders, followers and the Raft log."}))
            .unwrap();
        let params = SearchParams {
            attributes_to_highlight: Some(vec!["title".into()]),
            attributes_to_snippet: vec!["body:3".into()],
            ..SearchParams::new("raft")
        };
        let hit = &search(&index, &params).unwrap().hits[0];
        assert_eq!(hit.highlight["title"]["value"], "<em>Raft</em> explained");
        assert!(!hit.highlight.contains_key("body"));
        assert_eq!(hit.snippet["body"]["value"], "… the <em>Raft</em> log");

        // By default every searchable attribute is highlighted.
        let hit = &search(&index, &SearchParams::new("log")).unwrap().hits[0];
        assert_eq!(hit.highlight["body"]["value"], "Leaders, followers and the Raft <em>log</em>.");
        assert_eq!(hit.highlight["title"]["matchLevel"], "none");
    }
}
