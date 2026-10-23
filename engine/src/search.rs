//! Query execution (matching with typo tolerance; ranking comes later).
//!
//! Each query word is expanded into the dictionary words it can stand for:
//! itself, words within the allowed number of typos, and — for the last word,
//! which the user may still be typing — words that start with it (Algolia's
//! default `queryType: prefixLast`). A record matches when every query word
//! matches at least one of its expansions.

use crate::index::Index;
use crate::tokenizer::tokenize;
use crate::typo::{WordMatch, fuzzy_matches};
use roaring::RoaringBitmap;

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
}
