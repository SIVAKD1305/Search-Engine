//! Typo tolerance.
//!
//! A query word is compared with dictionary words using Damerau-Levenshtein
//! distance (the "optimal string alignment" variant: insert, delete, replace,
//! or swap two neighbouring letters, each costing 1).
//!
//! The dictionary is sorted, so neighbouring words share prefixes. We walk it
//! like a trie: the distance table rows for a shared prefix are reused, and as
//! soon as every cell of a row is above the allowed distance, no word with
//! that prefix can match and we jump past all of them. This is the "recursive
//! scan over the dictionary, guided by the maximum acceptable distance" that
//! Algolia describes.

use std::collections::BTreeMap;
use std::ops::Bound;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypoSettings {
    pub enabled: bool,
    /// Shorter query words must match exactly.
    pub min_word_size_for_1_typo: usize,
    pub min_word_size_for_2_typos: usize,
}

impl Default for TypoSettings {
    /// Algolia's defaults.
    fn default() -> Self {
        TypoSettings { enabled: true, min_word_size_for_1_typo: 4, min_word_size_for_2_typos: 8 }
    }
}

impl TypoSettings {
    /// Typos allowed for a query word of this many characters.
    pub fn max_typos(&self, chars: usize) -> u8 {
        if !self.enabled || chars < self.min_word_size_for_1_typo {
            0
        } else if chars < self.min_word_size_for_2_typos {
            1
        } else {
            2
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordMatch {
    pub word: String,
    pub typos: u8,
    /// True when the query matched only the start of `word` (search-as-you-type).
    pub prefix: bool,
}

/// Finds dictionary words within `max_typos` of `query`.
///
/// With `prefix`, a word also matches when `query` is close to its beginning,
/// so "mikc" finds "mickael". A different first letter costs one extra typo,
/// because users rarely get the first letter wrong.
pub fn fuzzy_matches<V>(
    dictionary: &BTreeMap<String, V>,
    query: &str,
    max_typos: u8,
    prefix: bool,
) -> Vec<WordMatch> {
    if max_typos == 0 {
        return exact_matches(dictionary, query, prefix);
    }
    let query: Vec<char> = query.chars().collect();
    let n = query.len();
    let limit = u32::from(max_typos);

    let mut matches = Vec::new();
    let mut word: Vec<char> = Vec::new();
    // Distance table, one row of `width` cells per character of `word`;
    // row 0 compares the query with the empty string.
    let width = n + 1;
    let mut table: Vec<u32> = (0..=n as u32).collect();
    let mut keys = dictionary.range::<str, _>(..);
    let mut chars: Vec<char> = Vec::new();

    'words: while let Some((key, _)) = keys.next() {
        chars.clear();
        chars.extend(key.chars());

        let shared = word.iter().zip(&chars).take_while(|(a, b)| a == b).count();
        word.truncate(shared);
        table.truncate((shared + 1) * width);

        // Best distance of the query against any prefix of this word.
        let mut best_prefix = if prefix {
            table.iter().skip(n).step_by(width).copied().min().unwrap()
        } else {
            u32::MAX
        };

        let mut complete = true;
        for &c in &chars[shared..] {
            word.push(c);
            push_row(&mut table, &word, &query);
            let row = &table[table.len() - width..];
            let row_min = *row.iter().min().unwrap();
            if prefix {
                best_prefix = best_prefix.min(row[n]);
            }
            if row_min > limit {
                if best_prefix <= limit {
                    // The query already matched a prefix of this word, and
                    // longer words cannot do better: the rest of the word
                    // does not matter.
                    complete = false;
                    break;
                }
                // Nothing that starts with `word` can match; skip all of it.
                match successor(&word) {
                    Some(next) => {
                        keys = dictionary.range::<str, _>((Bound::Included(next.as_str()), Bound::Unbounded));
                    }
                    None => break 'words,
                }
                continue 'words;
            }
        }

        let full = if complete { table[table.len() - 1] } else { u32::MAX };
        let (distance, is_prefix) =
            if prefix && best_prefix < full { (best_prefix, true) } else { (full, false) };
        let first_letter_penalty = u32::from(query.first() != chars.first());
        let typos = distance + if distance > 0 { first_letter_penalty } else { 0 };
        if typos <= limit {
            matches.push(WordMatch { word: key.to_owned(), typos: typos as u8, prefix: is_prefix });
        }
    }
    matches
}

fn exact_matches<V>(dictionary: &BTreeMap<String, V>, query: &str, prefix: bool) -> Vec<WordMatch> {
    if !prefix {
        return dictionary
            .get_key_value(query)
            .map(|(w, _)| WordMatch { word: w.clone(), typos: 0, prefix: false })
            .into_iter()
            .collect();
    }
    dictionary
        .range::<str, _>((Bound::Included(query), Bound::Unbounded))
        .map(|(w, _)| w)
        .take_while(|w| w.starts_with(query))
        .map(|w| WordMatch { word: w.clone(), typos: 0, prefix: w.len() > query.len() })
        .collect()
}

/// Appends the distance row for `word` (whose last char is new) against
/// `query`. The table holds one row per char of `word` minus one, plus row 0.
fn push_row(table: &mut Vec<u32>, word: &[char], query: &[char]) {
    let width = query.len() + 1;
    let k = word.len();
    let c = word[k - 1];
    let prev = (k - 1) * width;
    let cur = k * width;
    table.push(k as u32);
    for j in 1..width {
        let cost = u32::from(c != query[j - 1]);
        let mut d = (table[prev + j] + 1).min(table[cur + j - 1] + 1).min(table[prev + j - 1] + cost);
        if k > 1 && j > 1 && c == query[j - 2] && word[k - 2] == query[j - 1] {
            d = d.min(table[(k - 2) * width + j - 2] + 1);
        }
        table.push(d);
    }
}

/// The smallest string greater than every string starting with `prefix`
/// (UTF-8 byte order matches code point order), or `None` if there is none.
fn successor(prefix: &[char]) -> Option<String> {
    let mut chars = prefix.to_vec();
    while let Some(last) = chars.pop() {
        let mut code = last as u32 + 1;
        while code <= char::MAX as u32 {
            if let Some(next) = char::from_u32(code) {
                chars.push(next);
                return Some(chars.into_iter().collect());
            }
            code += 1; // skip the surrogate range
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(words: &[&str]) -> BTreeMap<String, ()> {
        words.iter().map(|w| (w.to_string(), ())).collect()
    }

    fn found(d: &BTreeMap<String, ()>, q: &str, max: u8, prefix: bool) -> Vec<(String, u8, bool)> {
        fuzzy_matches(d, q, max, prefix).into_iter().map(|m| (m.word, m.typos, m.prefix)).collect()
    }

    /// Plain O(n·m) reference implementation, used to check the trie walk.
    fn osa_distance(a: &str, b: &str) -> u32 {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut d = vec![vec![0u32; b.len() + 1]; a.len() + 1];
        for (i, row) in d.iter_mut().enumerate() {
            row[0] = i as u32;
        }
        for (j, cell) in d[0].iter_mut().enumerate() {
            *cell = j as u32;
        }
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                let cost = u32::from(a[i - 1] != b[j - 1]);
                d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
                }
            }
        }
        d[a.len()][b.len()]
    }

    #[test]
    fn thresholds_follow_algolia_defaults() {
        let t = TypoSettings::default();
        assert_eq!([3, 4, 7, 8, 20].map(|n| t.max_typos(n)), [0, 1, 1, 2, 2]);
        let off = TypoSettings { enabled: false, ..TypoSettings::default() };
        assert_eq!(off.max_typos(10), 0);
    }

    #[test]
    fn finds_insertions_deletions_replacements_and_swaps() {
        let d = dict(&["headphones", "wireless", "kettle", "keyboard"]);
        assert_eq!(found(&d, "wirless", 1, false), [("wireless".into(), 1, false)]);
        assert_eq!(found(&d, "wireles", 1, false), [("wireless".into(), 1, false)]);
        assert_eq!(found(&d, "wireleds", 1, false), [("wireless".into(), 1, false)]);
        assert_eq!(found(&d, "wrieless", 1, false), [("wireless".into(), 1, false)]);
        assert_eq!(found(&d, "hedphnes", 2, false), [("headphones".into(), 2, false)]);
        assert!(found(&d, "hedphnes", 1, false).is_empty());
    }

    #[test]
    fn exact_word_has_zero_typos() {
        let d = dict(&["kettle", "kettles"]);
        assert_eq!(found(&d, "kettle", 1, false), [("kettle".into(), 0, false), ("kettles".into(), 1, false)]);
    }

    #[test]
    fn first_letter_typo_costs_double() {
        let d = dict(&["kettle"]);
        assert!(found(&d, "bettle", 1, false).is_empty());
        assert_eq!(found(&d, "bettle", 2, false), [("kettle".into(), 2, false)]);
    }

    #[test]
    fn prefix_mode_matches_the_start_of_longer_words() {
        let d = dict(&["mickael", "michael", "mike", "music"]);
        // "mikc" vs "mick" is one swap; "mike" is one replacement of the last letter.
        assert_eq!(
            found(&d, "mikc", 1, true),
            [("michael".into(), 1, true), ("mickael".into(), 1, true), ("mike".into(), 1, false)]
        );
        assert_eq!(found(&d, "mi", 0, true).len(), 3);
        assert_eq!(found(&d, "mike", 0, true), [("mike".into(), 0, false)]);
    }

    #[test]
    fn trie_walk_agrees_with_reference_distance() {
        let words = [
            "head", "headphone", "headphones", "headset", "heat", "help", "hello", "helmet",
            "wireless", "wirelessly", "wire", "wise", "élan", "elan", "eland", "strasse",
        ];
        let d = dict(&words);
        for query in ["head", "haed", "helo", "wirless", "elan", "wise", "headpohnes", "strase"] {
            for max in [1u8, 2] {
                let mut expected: Vec<String> = words
                    .iter()
                    .filter(|w| {
                        let dist = osa_distance(query, w);
                        let penalty = u32::from(dist > 0 && query.chars().next() != w.chars().next());
                        dist + penalty <= u32::from(max)
                    })
                    .map(|w| w.to_string())
                    .collect();
                expected.sort();
                let got: Vec<String> = fuzzy_matches(&d, query, max, false).into_iter().map(|m| m.word).collect();
                assert_eq!(got, expected, "query {query:?} max {max}");
            }
        }
    }

    #[test]
    fn prefix_walk_agrees_with_reference_distance() {
        let words = [
            "head", "headphone", "headphones", "headset", "heat", "help", "hello", "helmet",
            "mickael", "michael", "mike", "music", "wireless", "wire", "wise", "kettle",
        ];
        let d = dict(&words);
        for query in ["hea", "haed", "headp", "mikc", "wirl", "kettel", "musci", "hlep"] {
            for max in [1u8, 2] {
                let mut expected: Vec<(String, u8)> = words
                    .iter()
                    .filter_map(|w| {
                        let prefixes: Vec<String> =
                            (1..=w.chars().count()).map(|k| w.chars().take(k).collect()).collect();
                        let dist = prefixes.iter().map(|p| osa_distance(query, p)).min().unwrap();
                        let penalty = u32::from(dist > 0 && query.chars().next() != w.chars().next());
                        (dist + penalty <= u32::from(max)).then(|| (w.to_string(), (dist + penalty) as u8))
                    })
                    .collect();
                expected.sort();
                let got: Vec<(String, u8)> =
                    fuzzy_matches(&d, query, max, true).into_iter().map(|m| (m.word, m.typos)).collect();
                assert_eq!(got, expected, "query {query:?} max {max}");
            }
        }
    }

    #[test]
    fn successor_skips_past_all_extensions() {
        assert_eq!(successor(&['a', 'b']).as_deref(), Some("ac"));
        assert_eq!(successor(&['\u{D7FF}']).as_deref(), Some("\u{E000}"));
        assert_eq!(successor(&['a', char::MAX]).as_deref(), Some("b"));
        assert_eq!(successor(&[char::MAX]), None);
    }
}
