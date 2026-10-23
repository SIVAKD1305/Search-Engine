//! Query execution (first version: matching only, no typos or ranking yet).
//!
//! Every query word must match. The last word is treated as a prefix, because
//! the user may still be typing it ("wireless head" finds "headphones"). This
//! is Algolia's default `queryType: prefixLast`.

use crate::index::Index;
use crate::tokenizer::tokenize;
use roaring::RoaringBitmap;

/// Records matching every word of `query`. An empty query matches everything.
pub fn candidates(index: &Index, query: &str) -> RoaringBitmap {
    let tokens = tokenize(query);
    let Some((last, rest)) = tokens.split_last() else {
        return index.all_docs();
    };

    // Intersect the rarest lists first so the working set shrinks quickly.
    let mut lists: Vec<&RoaringBitmap> = Vec::with_capacity(tokens.len());
    for token in rest {
        match index.word(&token.text) {
            Some(postings) => lists.push(&postings.docs),
            None => return RoaringBitmap::new(),
        }
    }
    match index.prefix_docs(&last.text) {
        Some(docs) => lists.push(docs),
        None => return RoaringBitmap::new(),
    }
    lists.sort_unstable_by_key(|docs| docs.len());

    let mut result = lists[0].clone();
    for docs in &lists[1..] {
        result &= *docs;
        if result.is_empty() {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Settings;
    use serde_json::json;

    fn index() -> Index {
        let mut index = Index::new(Settings::default());
        for (id, name) in [
            ("1", "Wireless Headphones"),
            ("2", "Wireless Mouse"),
            ("3", "Headphone Stand"),
            ("4", "Élan Kettle"),
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
    fn last_word_is_a_prefix_and_other_words_are_exact() {
        let index = index();
        assert_eq!(ids(&index, "wireless head"), ["1"]);
        assert_eq!(ids(&index, "head"), ["1", "3"]);
        // "head" is not a full word, so it does not match when followed by more words.
        assert!(ids(&index, "head wireless").is_empty());
    }

    #[test]
    fn queries_are_normalized_like_records() {
        let index = index();
        assert_eq!(ids(&index, "ELAN"), ["4"]);
        assert_eq!(ids(&index, "élan ket"), ["4"]);
    }

    #[test]
    fn unknown_words_match_nothing_and_empty_query_matches_all() {
        let index = index();
        assert!(ids(&index, "wireless toaster").is_empty());
        assert_eq!(ids(&index, "").len(), 4);
        assert_eq!(ids(&index, " ,, ").len(), 4);
    }
}
