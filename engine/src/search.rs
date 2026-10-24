//! Query execution: matching with typo tolerance, filters, facets and ranking.
//!
//! Each query word is expanded into the dictionary words it can stand for:
//! itself, words within the allowed number of typos, its plural or singular
//! (`ignorePlurals`), its synonyms, and — for the last word, which the user
//! may still be typing — words that start with it (Algolia's default
//! `queryType: prefixLast`). A record matches when every required query word
//! matches at least one of its expansions; `optionalWords` and
//! `removeWordsIfNoResults` relax that.

use crate::facets::FacetCount;
use crate::filter::{self, FilterError};
use crate::highlight::{self, HighlightTags, Matcher};
use crate::index::{Index, Settings};
use crate::language::{is_stop_word, plural_forms};
use crate::bucket::rank_page;
use crate::ranking::{RankingInfo, TermWords};
use crate::store::DocId;
use crate::tokenizer::{Token, group_parts, is_cjk_word, normalize, tokenize_with};
use crate::typo::{Matches, PrefixMatch, WordMatch, fuzzy_matches};
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::ops::Bound;

#[derive(Debug)]
pub struct QueryTerm {
    /// Normalized query word.
    pub text: String,
    pub is_prefix: bool,
    /// Records may match without this word; it only helps ranking.
    pub optional: bool,
    /// Single dictionary words this term can match, fewest typos first.
    pub matches: Vec<WordMatch>,
    /// Groups of words, by shared prefix, this term matches as a prefix.
    pub prefixes: Vec<PrefixMatch>,
    /// Records containing at least one of the matched words.
    pub docs: RoaringBitmap,
}

/// Algolia's `removeWordsIfNoResults`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoveWords {
    /// Return no results.
    #[default]
    None,
    /// Drop words from the end of the query until something matches.
    LastWords,
    /// Drop words from the start of the query until something matches.
    FirstWords,
    /// Make every word optional; records matching more words rank higher.
    AllOptional,
}

/// How query words are expanded: the index settings, possibly overridden
/// by the search request.
#[derive(Debug, Clone, Default)]
pub struct QueryOptions {
    pub ignore_plurals: bool,
    pub synonyms: bool,
    pub remove_stop_words: bool,
    /// Normalized optional words.
    pub optional_words: HashSet<String>,
}

impl QueryOptions {
    pub fn from_settings(settings: &Settings) -> Self {
        QueryOptions {
            ignore_plurals: settings.ignore_plurals,
            synonyms: true,
            remove_stop_words: settings.remove_stop_words,
            optional_words: settings.optional_words.iter().map(|w| normalize(w)).collect(),
        }
    }

    fn for_request(settings: &Settings, params: &SearchParams) -> Self {
        let mut options = QueryOptions::from_settings(settings);
        options.ignore_plurals = params.ignore_plurals.unwrap_or(options.ignore_plurals);
        options.synonyms = params.synonyms.unwrap_or(true);
        options.remove_stop_words = params.remove_stop_words.unwrap_or(options.remove_stop_words);
        if let Some(words) = &params.optional_words {
            options.optional_words = words.iter().map(|w| normalize(w)).collect();
        }
        options
    }
}

/// Expands every query word, using the index settings.
pub fn plan(index: &Index, query: &str) -> Vec<QueryTerm> {
    plan_with(index, query, &QueryOptions::from_settings(index.settings()))
}

pub fn plan_with(index: &Index, query: &str, options: &QueryOptions) -> Vec<QueryTerm> {
    let mut words = group_parts(tokenize_with(query, index.segmentation().with_parts()));
    let last = words.len().saturating_sub(1);
    if options.remove_stop_words {
        // The last word is kept: the user may still be typing a longer word.
        let mut i = 0;
        words.retain(|(t, _)| {
            let keep = i == last || !is_stop_word(&t.text);
            i += 1;
            keep
        });
    }
    join_cut_words(index, &mut words, options);
    let last = words.len().saturating_sub(1);
    let mut terms = Vec::with_capacity(words.len());
    for (i, (token, parts)) in words.into_iter().enumerate() {
        let is_prefix = i == last;
        let term = plan_term(index, token.text.clone(), is_prefix, options);
        if term.matches.is_empty() && term.prefixes.is_empty() && is_cjk_word(&token.text) {
            // A Chinese or Japanese word that no record contains as a whole:
            // look for its parts instead ("国際空港" in "関西国際空港").
            if let Some(cover) = covering_parts(&token, parts) {
                let n = cover.len();
                for (j, part) in cover.into_iter().enumerate() {
                    terms.push(plan_term(index, part.text, is_prefix && j + 1 == n, options));
                }
                continue;
            }
            if is_prefix && let Some((head, tail)) = split_typed_word(index, &token.text, options) {
                terms.push(head);
                terms.push(tail);
                continue;
            }
        }
        terms.push(term);
    }
    terms
}

/// A half-typed last word that splits coarser than the records: "ペンチオ"
/// is one word, but the record has ペンチ + オノマトペ. Splits it into a whole
/// word and the start of the next, both in one record; the longest whole
/// word wins.
fn split_typed_word(index: &Index, text: &str, options: &QueryOptions) -> Option<(QueryTerm, QueryTerm)> {
    let ends: Vec<usize> = text.char_indices().map(|(i, _)| i).skip(1).collect();
    for &end in ends.iter().rev() {
        if index.word(&text[..end]).is_none() {
            continue;
        }
        let tail = plan_term(index, text[end..].to_owned(), true, options);
        let head = plan_term(index, text[..end].to_owned(), false, options);
        if !head.docs.is_disjoint(&tail.docs) {
            return Some((head, tail));
        }
    }
    None
}

/// While Chinese or Japanese is typed, the last word is often cut short, and
/// a cut word can split differently from the whole one: "セルフタイマ" gives
/// セルフ + タイマ, while "セルフタイマー" is one word. When no record has
/// all the words of the query's last unspaced run, joins the run's trailing
/// words back into one, if that starts a word in the index (the longest such
/// join wins).
fn join_cut_words(index: &Index, words: &mut Vec<(Token, Vec<Token>)>, options: &QueryOptions) {
    let Some(last) = words.len().checked_sub(1) else { return };
    let joinable = |a: &Token, b: &Token| a.end == b.start && is_cjk_word(&a.text) && is_cjk_word(&b.text);
    let mut first = last;
    while first > 0 && joinable(&words[first - 1].0, &words[first].0) {
        first -= 1;
    }
    if first == last {
        return;
    }
    // Keep the split when some record matches it.
    let mut docs: Option<RoaringBitmap> = None;
    for (i, (token, _)) in words.iter().enumerate().skip(first) {
        let term_docs = plan_term(index, token.text.clone(), i == last, options).docs;
        docs = Some(match docs {
            None => term_docs,
            Some(d) => d & term_docs,
        });
    }
    if docs.is_some_and(|d| !d.is_empty()) {
        return;
    }
    for i in first..last {
        let joined: String = words[i..].iter().map(|(t, _)| t.text.as_str()).collect();
        let starts_a_word = index
            .dictionary()
            .range::<str, _>((Bound::Included(joined.as_str()), Bound::Unbounded))
            .next()
            .is_some_and(|(w, _)| w.starts_with(&joined));
        if starts_a_word {
            let (start, position) = (words[i].0.start, words[i].0.position);
            let end = words[last].0.end;
            words.truncate(i);
            words.push((Token { text: joined, position, element: 0, start, end, part: false }, Vec::new()));
            return;
        }
    }
}

/// Parts that cover the whole token without overlapping, longest first at
/// each step; `None` unless there are at least two.
fn covering_parts(token: &Token, mut parts: Vec<Token>) -> Option<Vec<Token>> {
    parts.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut cover: Vec<Token> = Vec::new();
    let mut cursor = token.start;
    for part in parts {
        if part.start == cursor {
            cursor = part.end;
            cover.push(part);
        }
    }
    (cursor == token.end && cover.len() >= 2).then_some(cover)
}

fn plan_term(index: &Index, text: String, is_prefix: bool, options: &QueryOptions) -> QueryTerm {
    // A different Chinese or Japanese character is a different word, not a typo.
    let max_typos = if is_cjk_word(&text) { 0 } else { index.settings().typo.max_typos(text.chars().count()) };
    let Matches { words: mut matches, prefixes } = fuzzy_matches(index.dictionary(), &text, max_typos, is_prefix);

    let mut alternatives: Vec<String> = Vec::new();
    if options.ignore_plurals {
        alternatives.extend(plural_forms(&text));
    }
    if options.synonyms {
        alternatives.extend(index.synonyms_of(&text).iter().cloned());
    }
    for word in alternatives {
        if word == text || index.word(&word).is_none() {
            continue;
        }
        // A plural found as a typo or prefix match counts as no typo.
        matches.retain(|m| m.word != word);
        matches.push(WordMatch { word, typos: 0, prefix: false, derived: true });
    }

    matches.sort_by(|a, b| a.typos.cmp(&b.typos).then_with(|| a.word.cmp(&b.word)));
    let docs = term_docs(index, &matches, &prefixes);
    let optional = options.optional_words.contains(&text);
    QueryTerm { text, is_prefix, optional, matches, prefixes, docs }
}

fn term_docs(index: &Index, matches: &[WordMatch], prefixes: &[PrefixMatch]) -> RoaringBitmap {
    let mut docs = RoaringBitmap::new();
    // A prefix group is one precomputed set, however many words it holds.
    for p in prefixes {
        if let Some(prefix_docs) = index.prefix_docs(&p.prefix) {
            docs |= prefix_docs.as_ref();
        }
    }
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
    /// More required conditions, each parsed on its own and combined with
    /// `filters` by AND, so no `filters` text can lift them. Set by the
    /// server from API key restrictions; never read from a request.
    #[serde(skip)]
    pub enforced_filters: Vec<String>,
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
    // The following override the index settings of the same name.
    pub ignore_plurals: Option<bool>,
    pub remove_stop_words: Option<bool>,
    /// `false` turns synonyms off for this request.
    pub synonyms: Option<bool>,
    pub remove_words_if_no_results: Option<RemoveWords>,
    /// A list, or one string of words separated by spaces or commas.
    #[serde(deserialize_with = "words_list")]
    pub optional_words: Option<Vec<String>>,
}

fn words_list<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Words {
        One(String),
        Many(Vec<String>),
    }
    Ok(Option::<Words>::deserialize(deserializer)?.map(|words| match words {
        Words::One(s) => s.split([' ', ',']).filter(|w| !w.is_empty()).map(str::to_owned).collect(),
        Words::Many(list) => list,
    }))
}

impl Default for SearchParams {
    fn default() -> Self {
        SearchParams {
            query: String::new(),
            filters: None,
            enforced_filters: Vec::new(),
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
            ignore_plurals: None,
            remove_stop_words: None,
            synonyms: None,
            remove_words_if_no_results: None,
            optional_words: None,
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
    /// The query words used, after any removal.
    pub terms: Vec<QueryTerm>,
    /// Set when words were removed: the query with removed words wrapped
    /// in the highlight tags (Algolia's `queryAfterRemoval`).
    pub query_after_removal: Option<String>,
}

/// Runs a query and returns one page (0-based) of ranked results.
pub fn search(index: &Index, params: &SearchParams) -> Result<SearchResults, FilterError> {
    let hits_per_page = params.hits_per_page.max(1);
    let options = QueryOptions::for_request(index.settings(), params);
    let mut terms = plan_with(index, &params.query, &options);
    let mut filtered: Option<RoaringBitmap> = None;
    for filters in params.filters.iter().chain(&params.enforced_filters).filter(|f| !f.trim().is_empty()) {
        let docs = filter::parse(filters)?.evaluate(index)?;
        filtered = Some(match filtered {
            Some(f) => f & docs,
            None => docs,
        });
    }
    let apply_filters = |mut docs: RoaringBitmap| {
        if let Some(f) = &filtered {
            docs &= f;
        }
        docs
    };
    let mut candidates = apply_filters(matching(index, &terms));

    let strategy = params.remove_words_if_no_results.unwrap_or(index.settings().remove_words_if_no_results);
    let mut removed: Vec<String> = Vec::new();
    if candidates.is_empty() && !terms.is_empty() {
        match strategy {
            RemoveWords::None => {}
            RemoveWords::AllOptional => {
                for term in &mut terms {
                    term.optional = true;
                }
                candidates = apply_filters(matching(index, &terms));
            }
            RemoveWords::LastWords | RemoveWords::FirstWords => {
                while candidates.is_empty() && terms.iter().filter(|t| !t.optional).count() > 1 {
                    let position = if strategy == RemoveWords::LastWords {
                        terms.iter().rposition(|t| !t.optional)
                    } else {
                        terms.iter().position(|t| !t.optional)
                    };
                    let term = terms.remove(position.expect("a required term exists"));
                    removed.push(term.text);
                    candidates = apply_filters(matching(index, &terms));
                }
            }
        }
    }
    let query_after_removal = (!removed.is_empty()).then(|| {
        tokenize_with(&params.query, index.segmentation())
            .iter()
            .map(|t| {
                let original = &params.query[t.start..t.end];
                if removed.contains(&t.text) {
                    format!("{}{original}{}", params.highlight_pre_tag, params.highlight_post_tag)
                } else {
                    original.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    });
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

    let nb_hits = candidates.len();
    let offset = params.page.saturating_mul(hits_per_page);
    let page = rank_page(index, &terms, &candidates, &boosts, offset, hits_per_page).into_iter();

    let matcher = Matcher::new(&terms, index.segmentation());
    // Which attributes of a record hold a matched word, from the record's
    // word list, so highlighting can skip the others.
    let term_words: Vec<TermWords> = terms.iter().map(|t| TermWords::new(index, t)).collect();
    let matched_attributes = |doc: DocId| -> Vec<bool> {
        let mut matched = vec![false; index.attributes().len()];
        for o in index.occurrences(doc) {
            if term_words.iter().any(|w| w.get(o.word).is_some()) {
                matched[usize::from(o.attribute)] = true;
            }
        }
        matched
    };
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
            let matched = matched_attributes(r.doc);
            // Attributes that are not searchable are not in the word list.
            let may_match =
                |name: &str| index.attributes().iter().position(|a| a == name).is_none_or(|i| matched[i]);
            SearchHit {
                doc: r.doc,
                info: r.info,
                highlight: highlight::highlight(&record, &to_highlight, &matcher, &tags, may_match),
                snippet: highlight::snippet(&record, &params.attributes_to_snippet, &matcher, &tags),
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
        query_after_removal,
    })
}

/// Records matching every word of `query`. An empty query matches everything.
pub fn candidates(index: &Index, query: &str) -> RoaringBitmap {
    matching(index, &plan(index, query))
}

/// Records matching every required term. If every term is optional, records
/// matching at least one term.
pub fn matching(index: &Index, terms: &[QueryTerm]) -> RoaringBitmap {
    if terms.is_empty() {
        return index.all_docs();
    }
    let required: Vec<&QueryTerm> = terms.iter().filter(|t| !t.optional).collect();
    if required.is_empty() {
        let mut union = RoaringBitmap::new();
        for term in terms {
            union |= &term.docs;
        }
        return union;
    }
    // Intersect the smallest sets first so the working set shrinks quickly.
    let mut sets: Vec<&RoaringBitmap> = required.iter().map(|t| &t.docs).collect();
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
        let expanded = |term: &QueryTerm| -> Vec<(String, u8, bool)> {
            let all = Matches { words: term.matches.clone(), prefixes: term.prefixes.clone() };
            all.expand(index.dictionary()).into_iter().map(|m| (m.word, m.typos, m.prefix)).collect()
        };
        // Still typing: "headphones" matches as a prefix group, and the
        // whole word "headphone" stays an exact match.
        let terms = plan(&index, "headphone ");
        assert_eq!(terms[0].prefixes, [PrefixMatch { prefix: "headphone".into(), typos: 0 }]);
        assert_eq!(
            expanded(&terms[0]),
            [("headphone".into(), 0, false), ("headphones".into(), 0, true)]
        );
        // A finished word: "headphones" is one typo away.
        let terms = plan(&index, "headphone stand");
        assert!(terms[0].prefixes.is_empty());
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
    fn enforced_filters_cannot_be_lifted_by_the_request() {
        let index = shop();
        let ids = |filters: Option<&str>| -> Result<Vec<String>, FilterError> {
            let params = SearchParams {
                filters: filters.map(Into::into),
                enforced_filters: vec!["brand:Orbix".into()],
                ..SearchParams::new("")
            };
            Ok(search(&index, &params)?
                .hits
                .iter()
                .map(|h| index.store().get(h.doc).unwrap()["objectID"].as_str().unwrap().to_owned())
                .collect())
        };
        assert_eq!(ids(None).unwrap(), ["2"]);
        assert_eq!(ids(Some("brand:Élan OR brand:Orbix")).unwrap(), ["2"]);
        assert!(ids(Some("brand:Élan")).unwrap().is_empty());
        // Text that would escape a filter joined into one string with AND.
        assert!(ids(Some("brand:Élan) OR (brand:Élan")).is_err());
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
        let order: Vec<String> = results
            .hits
            .iter()
            .map(|h| index.store().get(h.doc).unwrap()["objectID"].as_str().unwrap().to_owned())
            .collect();
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

#[cfg(test)]
mod query_tests {
    use super::*;
    use serde_json::json;

    fn shop(extra: Value) -> Index {
        let mut settings = json!({"searchableAttributes": ["name", "description"], "customRanking": ["desc(rating)"]});
        settings.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let mut index = Index::new(serde_json::from_value(settings).unwrap());
        for r in [
            json!({"objectID": "1", "name": "Wireless Headphones", "rating": 4}),
            json!({"objectID": "2", "name": "Headphone Stand", "rating": 5}),
            json!({"objectID": "3", "name": "Television 55 inch", "rating": 3}),
            json!({"objectID": "4", "name": "Smartphone Case", "rating": 2}),
            json!({"objectID": "5", "name": "Lamp", "description": "The light of the house", "rating": 1}),
            json!({"objectID": "6", "name": "Battery Pack", "rating": 1}),
        ] {
            index.add(r).unwrap();
        }
        index
    }

    fn run(index: &Index, params: SearchParams) -> (Vec<String>, SearchResults) {
        let results = search(index, &params).unwrap();
        let ids = results
            .hits
            .iter()
            .map(|h| index.store().get(h.doc).unwrap()["objectID"].as_str().unwrap().to_owned())
            .collect();
        (ids, results)
    }

    fn ids(index: &Index, query: &str) -> Vec<String> {
        run(index, SearchParams::new(query)).0
    }

    #[test]
    fn plurals_match_without_counting_as_typos_or_exact() {
        let index = shop(json!({"ignorePlurals": true}));
        let (found, results) = run(&index, SearchParams::new("headphone stand"));
        assert_eq!(found, ["2"]);
        let (found, results2) = run(&index, SearchParams::new("headphone "));
        assert_eq!(found, ["2", "1"]);
        // "headphones" is a plural, not a typo, and not an exact match.
        let hit = results2.hits.iter().find(|h| h.doc == 0).unwrap();
        assert_eq!((hit.info.nb_typos, hit.info.nb_exact_words), (0, 0));
        assert_eq!(ids(&index, "batteries"), ["6"]);
        assert!(results.query_after_removal.is_none());
        // Off by default, and per request.
        assert!(ids(&shop(json!({})), "batteries").is_empty());
        let (found, _) = run(&index, SearchParams { ignore_plurals: Some(false), ..SearchParams::new("batteries") });
        assert!(found.is_empty());
    }

    #[test]
    fn synonyms_expand_query_words_and_are_highlighted() {
        let index = shop(json!({"synonyms": [
            {"type": "synonym", "synonyms": ["tv", "television"]},
            {"type": "oneWaySynonym", "input": "phone", "synonyms": ["smartphone"]}
        ]}));
        let (found, results) = run(&index, SearchParams::new("tv"));
        assert_eq!(found, ["3"]);
        assert_eq!(results.hits[0].highlight["name"]["value"], "<em>Television</em> 55 inch");
        assert_eq!(ids(&index, "phone case"), ["4"]);
        // One-way: "smartphone" does not find "phone".
        assert_eq!(ids(&index, "smartphone"), ["4"]);
        let (found, _) = run(&index, SearchParams { synonyms: Some(false), ..SearchParams::new("tv") });
        assert!(found.is_empty());
    }

    #[test]
    fn stop_words_are_removed_except_the_last_word() {
        let index = shop(json!({"removeStopWords": true}));
        assert_eq!(ids(&index, "the light of house"), ["5"]);
        // Without removal, "of" and "the" must match too (they do here).
        assert_eq!(ids(&shop(json!({})), "the light of house"), ["5"]);
        // "the" is not in any name, so it would exclude everything.
        assert_eq!(ids(&index, "the lamp"), ["5"]);
        assert!(ids(&shop(json!({})), "the wireless").is_empty());
        assert_eq!(ids(&index, "the wireless"), ["1"]);
        // A trailing stop word is kept as a prefix ("the" could become "theater").
        assert!(ids(&index, "wireless the").is_empty());
    }

    #[test]
    fn remove_words_if_no_results_strategies() {
        let index = shop(json!({}));
        assert!(ids(&index, "wireless headphones toaster").is_empty());

        let last = SearchParams { remove_words_if_no_results: Some(RemoveWords::LastWords), ..SearchParams::new("wireless headphones toaster") };
        let (found, results) = run(&index, last);
        assert_eq!(found, ["1"]);
        assert_eq!(results.query_after_removal.as_deref(), Some("wireless headphones <em>toaster</em>"));
        assert_eq!(results.terms.len(), 2);

        let first = SearchParams { remove_words_if_no_results: Some(RemoveWords::FirstWords), ..SearchParams::new("toaster battery pack") };
        let (found, results) = run(&index, first);
        assert_eq!(found, ["6"]);
        assert_eq!(results.query_after_removal.as_deref(), Some("<em>toaster</em> battery pack"));

        // allOptional: more matched words rank first.
        let all = SearchParams { remove_words_if_no_results: Some(RemoveWords::AllOptional), ..SearchParams::new("wireless stand toaster") };
        let (found, results) = run(&index, all);
        assert_eq!(found, ["2", "1"]);
        assert_eq!(results.hits[0].info.words, 1);

        // The setting applies when the request does not override it.
        let index = shop(json!({"removeWordsIfNoResults": "lastWords"}));
        assert_eq!(ids(&index, "lamp toaster"), ["5"]);
        // A query that already matches is never changed.
        let (_, results) = run(&index, SearchParams::new("lamp"));
        assert!(results.query_after_removal.is_none());
    }

    #[test]
    fn optional_words_boost_but_are_not_required() {
        let index = shop(json!({"optionalWords": ["wireless"]}));
        assert_eq!(ids(&index, "wireless headphone "), ["1", "2"]);
        let (_, results) = run(&index, SearchParams::new("wireless headphone "));
        assert_eq!(results.hits[0].info.words, 2);
        assert_eq!(results.hits[1].info.words, 1);
        let (found, _) = run(&index, SearchParams { optional_words: Some(vec![]), ..SearchParams::new("wireless headphone ") });
        assert_eq!(found, ["1"]);
    }

    #[test]
    fn optional_words_accept_a_string_or_a_list() {
        let p: SearchParams = serde_json::from_value(json!({"optionalWords": "new, york city"})).unwrap();
        assert_eq!(p.optional_words.unwrap(), ["new", "york", "city"]);
        let p: SearchParams = serde_json::from_value(json!({"optionalWords": ["a b"]})).unwrap();
        assert_eq!(p.optional_words.unwrap(), ["a b"]);
        let p: SearchParams = serde_json::from_value(json!({})).unwrap();
        assert!(p.optional_words.is_none());
    }

    #[test]
    fn new_settings_round_trip_with_algolia_names() {
        let settings: Settings = serde_json::from_value(json!({
            "ignorePlurals": true, "removeStopWords": true, "removeWordsIfNoResults": "allOptional",
            "optionalWords": ["new"], "synonyms": [{"type": "synonym", "synonyms": ["a1", "b1"]}]
        }))
        .unwrap();
        let back = serde_json::to_value(&settings).unwrap();
        assert_eq!(back["removeWordsIfNoResults"], "allOptional");
        assert_eq!(back["synonyms"][0]["type"], "synonym");
        assert!(serde_json::from_value::<Settings>(json!({"removeWordsIfNoResults": "some"})).is_err());
    }
}

#[cfg(test)]
mod highlight_shortcut_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn skipping_unmatched_attributes_changes_nothing() {
        let settings: Settings = serde_json::from_value(json!({
            "searchableAttributes": ["name", "tags", "description"]
        }))
        .unwrap();
        let mut index = Index::new(settings);
        let words = ["wireless", "head", "headphones", "black", "kettle", "Élan", "steel", "lamp"];
        for id in 0..200usize {
            let pick = |k: usize| words[(id * 7 + k * 3) % words.len()];
            index
                .add(json!({
                    "objectID": id.to_string(),
                    "name": format!("{} {}", pick(0), pick(1)),
                    "tags": [pick(2), pick(3)],
                    "description": format!("A {} with a {} & <b>{}</b>", pick(4), pick(5), pick(6)),
                    "brand": pick(7),
                }))
                .unwrap();
        }
        for query in ["wirless", "head", "elan ste", "black kettle", "lamp", "zzz", ""] {
            let params = SearchParams {
                attributes_to_highlight: Some(vec!["name".into(), "tags".into(), "description".into(), "brand".into()]),
                hits_per_page: 50,
                ..SearchParams::new(query)
            };
            let results = search(&index, &params).unwrap();
            let matcher = Matcher::new(&results.terms, index.segmentation());
            for hit in &results.hits {
                let record = index.store().get(hit.doc).unwrap();
                let attributes = params.attributes_to_highlight.as_ref().unwrap();
                let full = highlight::highlight(&record, attributes, &matcher, &HighlightTags::default(), |_| true);
                assert_eq!(hit.highlight, full, "query {query:?} record {}", record["objectID"]);
            }
        }
    }
}


#[cfg(all(test, feature = "chinese", feature = "japanese"))]
mod cjk_tests {
    use super::*;
    use serde_json::json;

    fn index(languages: &[&str], names: &[&str]) -> Index {
        let settings = json!({"searchableAttributes": ["name"], "indexLanguages": languages});
        let mut index = Index::new(serde_json::from_value(settings).unwrap());
        for (i, name) in names.iter().enumerate() {
            index.add(json!({"objectID": (i + 1).to_string(), "name": name})).unwrap();
        }
        index
    }

    /// objectIDs of the hits.
    fn ids(index: &Index, query: &str) -> Vec<String> {
        search(index, &SearchParams::new(query))
            .unwrap()
            .hits
            .iter()
            .map(|h| index.store().get(h.doc).unwrap()["objectID"].as_str().unwrap().to_owned())
            .collect()
    }

    fn highlighted(index: &Index, query: &str) -> String {
        let results = search(index, &SearchParams::new(query)).unwrap();
        results.hits[0].highlight["name"]["value"].as_str().unwrap().to_owned()
    }

    #[test]
    fn japanese_words_parts_and_prefixes_are_found() {
        let shop = index(&["ja"], &["関西国際空港限定トートバッグ", "東京タワーの置物", "ワイヤレスイヤホン Bluetooth対応", "ハックの本"]);
        assert_eq!(ids(&shop, "トートバッグ"), ["1"]);
        // A part of a compound word, and a word made of two parts.
        assert_eq!(ids(&shop, "空港"), ["1"]);
        assert_eq!(ids(&shop, "国際空港"), ["1"]);
        // Kanji only, split as Japanese because the index says so.
        assert_eq!(ids(&shop, "対応"), ["3"]);
        // Typing: the last word is a prefix.
        assert_eq!(ids(&shop, "東京タ"), ["2"]);
        // Voiced kana are different letters: バッグ is not ハック.
        assert_eq!(ids(&shop, "ハック"), ["4"]);
        // No typo tolerance: one different character is another word.
        assert!(ids(&shop, "東高タワー").is_empty());
        assert_eq!(highlighted(&shop, "空港"), "関西国際<em>空港</em>限定トートバッグ");
        assert_eq!(highlighted(&shop, "東京 タワー"), "<em>東京</em><em>タワー</em>の置物");
    }

    #[test]
    fn chinese_words_and_parts_are_found() {
        let shop = index(&[], &["无线蓝牙耳机降噪", "清华大学出版社", "蓝牙音箱"]);
        assert_eq!(ids(&shop, "蓝牙耳机"), ["1"]);
        assert_eq!(ids(&shop, "大学"), ["2"]);
        let mut both = ids(&shop, "蓝牙");
        both.sort();
        assert_eq!(both, ["1", "3"]);
        assert_eq!(highlighted(&shop, "大学"), "清华<em>大学</em>出版社");
    }

    #[test]
    fn a_word_no_record_contains_whole_is_searched_by_its_parts() {
        let shop = index(&[], &["清华 大学 出版社", "北京大学"]);
        let terms = plan(&shop, "清华大学");
        assert_eq!(terms.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(), ["清华", "大学"]);
        assert!(!terms[0].is_prefix && terms[1].is_prefix);
        assert_eq!(ids(&shop, "清华大学"), ["1"]);
        // A word the records do contain is not split.
        assert_eq!(plan(&shop, "北京大学").len(), 1);
    }

    #[test]
    fn a_word_cut_short_while_typing_is_joined_back() {
        let shop = index(&["ja"], &["セルフタイマー付きカメラ", "インターナショナルスクール"]);
        let words = |q: &str| plan(&shop, q).into_iter().map(|t| t.text).collect::<Vec<_>>();
        assert_eq!(words("セルフタイマ"), ["セルフタイマ"]);
        assert_eq!(ids(&shop, "セルフタイマ"), ["1"]);
        assert_eq!(ids(&shop, "インターナシ"), ["2"]);
        // Words that match as they are stay split.
        assert_eq!(words("セルフタイマー付き").len(), 2);
        let zh = index(&[], &["目瞪口呆的表情", "日内瓦湖"]);
        assert_eq!(ids(&zh, "目瞪口"), ["1"]);
        assert_eq!(ids(&zh, "日内"), ["2"]);
    }

    #[test]
    fn a_typed_word_that_spans_two_record_words_is_split() {
        let shop = index(&["ja"], &["ペンチオノマトペです"]);
        assert_eq!(plan(&shop, "ペンチオ").into_iter().map(|t| t.text).collect::<Vec<_>>(), ["ペンチ", "オ"]);
        assert_eq!(ids(&shop, "ペンチオ"), ["1"]);
        let zh = index(&[], &["一个我们一个套装"]);
        assert_eq!(ids(&zh, "一个我们一个套"), ["1"]);
    }

    #[test]
    fn full_width_letters_match_ascii_queries() {
        let shop = index(&["ja"], &["Ｗｉｆｉ対応ルーター"]);
        assert_eq!(ids(&shop, "wifi"), ["1"]);
        assert_eq!(ids(&shop, "ルーター"), ["1"]);
    }
}
