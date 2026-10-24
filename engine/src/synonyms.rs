//! Synonyms, in Algolia's record format:
//!
//! ```json
//! {"type": "synonym", "synonyms": ["tv", "television"]}
//! {"type": "oneWaySynonym", "input": "phone", "synonyms": ["smartphone"]}
//! ```
//!
//! A regular synonym makes every entry stand for every other one; a one-way
//! synonym only expands `input`. Entries may be several words
//! (`["ny", "new york"]`): a query word can then match the phrase (its
//! words next to each other in one attribute), and query words that form an
//! entry can match its single-word or multi-word alternatives.

use crate::tokenizer::{Segmentation, tokenize_with};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Longest synonym entry, in words.
pub const MAX_SYNONYM_WORDS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", try_from = "RawSynonym")]
pub enum Synonym {
    Synonym { synonyms: Vec<String> },
    OneWaySynonym { input: String, synonyms: Vec<String> },
}

/// The shape as written, before validation.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum RawSynonym {
    Synonym {
        synonyms: Vec<String>,
    },
    OneWaySynonym {
        input: String,
        synonyms: Vec<String>,
    },
}

/// Normalizes a synonym entry into its words (Chinese and Japanese are
/// split like any text, so "笔记本电脑" is two words).
fn phrase(entry: &str, segmentation: Segmentation) -> Result<Vec<String>, String> {
    let words: Vec<String> = tokenize_with(entry, segmentation).into_iter().map(|t| t.text).collect();
    match words.len() {
        0 => Err(format!("empty synonym {entry:?}")),
        n if n > MAX_SYNONYM_WORDS => Err(format!("synonym {entry:?} has more than {MAX_SYNONYM_WORDS} words")),
        _ => Ok(words),
    }
}

impl TryFrom<RawSynonym> for Synonym {
    type Error = String;

    fn try_from(raw: RawSynonym) -> Result<Self, Self::Error> {
        let check = |words: &[String], min: usize| -> Result<(), String> {
            if words.len() < min {
                return Err(format!("a synonym needs at least {min} word(s)"));
            }
            words.iter().try_for_each(|w| phrase(w, Segmentation::default()).map(|_| ()))
        };
        match raw {
            RawSynonym::Synonym { synonyms } => {
                check(&synonyms, 2)?;
                Ok(Synonym::Synonym { synonyms })
            }
            RawSynonym::OneWaySynonym { input, synonyms } => {
                phrase(&input, Segmentation::default())?;
                check(&synonyms, 1)?;
                Ok(Synonym::OneWaySynonym { input, synonyms })
            }
        }
    }
}

/// Normalized phrase (its words) -> the phrases it can stand for.
#[derive(Debug, Clone, Default)]
pub struct SynonymMap {
    map: HashMap<Vec<String>, Vec<Vec<String>>>,
    /// Longest phrase with alternatives, in words.
    longest: usize,
}

impl SynonymMap {
    pub fn get(&self, words: &[String]) -> &[Vec<String>] {
        self.map.get(words).map_or(&[], Vec::as_slice)
    }

    pub fn longest(&self) -> usize {
        self.longest
    }
}

pub fn build_map(synonyms: &[Synonym], segmentation: Segmentation) -> SynonymMap {
    let mut map: HashMap<Vec<String>, Vec<Vec<String>>> = HashMap::new();
    let normalized =
        |entries: &[String]| -> Vec<Vec<String>> { entries.iter().filter_map(|e| phrase(e, segmentation).ok()).collect() };
    for synonym in synonyms {
        match synonym {
            Synonym::Synonym { synonyms } => {
                let phrases = normalized(synonyms);
                for from in &phrases {
                    let targets = map.entry(from.clone()).or_default();
                    targets.extend(phrases.iter().filter(|p| *p != from).cloned());
                }
            }
            Synonym::OneWaySynonym { input, synonyms } => {
                if let Ok(from) = phrase(input, segmentation) {
                    map.entry(from.clone()).or_default().extend(normalized(synonyms).into_iter().filter(|p| *p != from));
                }
            }
        }
    }
    map.retain(|_, targets| {
        targets.sort();
        targets.dedup();
        !targets.is_empty()
    });
    let longest = map.keys().map(Vec::len).max().unwrap_or(0);
    SynonymMap { map, longest }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: serde_json::Value) -> Result<Synonym, String> {
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    #[test]
    fn parses_and_expands_both_kinds() {
        let s = vec![
            parse(json!({"type": "synonym", "synonyms": ["TV", "Télévision", "telly"]})).unwrap(),
            parse(json!({"type": "oneWaySynonym", "input": "phone", "synonyms": ["smartphone"]})).unwrap(),
        ];
        let map = build_map(&s, Segmentation::default());
        let get = |words: &[&str]| -> Vec<String> {
            let words: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            map.get(&words).iter().map(|p| p.join(" ")).collect()
        };
        assert_eq!(get(&["tv"]), ["television", "telly"]);
        assert_eq!(get(&["television"]), ["telly", "tv"]);
        assert_eq!(get(&["phone"]), ["smartphone"]);
        assert!(get(&["smartphone"]).is_empty());
        assert_eq!(map.longest(), 1);
    }

    #[test]
    fn entries_may_be_phrases() {
        let s = vec![
            parse(json!({"type": "synonym", "synonyms": ["NY", "New York", "nyc"]})).unwrap(),
            parse(json!({"type": "oneWaySynonym", "input": "laptop", "synonyms": ["notebook computer"]})).unwrap(),
            parse(json!({"type": "synonym", "synonyms": ["笔记本电脑", "手提电脑"]})).unwrap(),
        ];
        let map = build_map(&s, Segmentation::default());
        let words = |text: &str| -> Vec<String> { text.split(' ').map(str::to_owned).collect() };
        assert_eq!(map.get(&words("ny")), [words("new york"), words("nyc")]);
        assert_eq!(map.get(&words("new york")), [words("ny"), words("nyc")]);
        assert_eq!(map.get(&words("laptop")), [words("notebook computer")]);
        assert!(map.get(&words("notebook computer")).is_empty());
        // Chinese entries are split into words like any text.
        let laptop = phrase("笔记本电脑", Segmentation::default()).unwrap();
        assert_eq!(map.get(&laptop), [phrase("手提电脑", Segmentation::default()).unwrap()]);
        assert_eq!(map.longest(), 2.max(laptop.len()));
    }

    #[test]
    fn rejects_invalid_synonyms() {
        assert!(parse(json!({"type": "synonym", "synonyms": ["tv"]})).unwrap_err().contains("at least 2"));
        assert!(parse(json!({"type": "synonym", "synonyms": ["a b c d e f", "x"]})).unwrap_err().contains("more than 5"));
        assert!(parse(json!({"type": "oneWaySynonym", "input": "", "synonyms": ["x"]})).unwrap_err().contains("empty"));
        assert!(parse(json!({"type": "placeholder"})).is_err());
    }

    #[test]
    fn serializes_back_to_algolia_format() {
        let s = parse(json!({"type": "oneWaySynonym", "input": "phone", "synonyms": ["smartphone"]})).unwrap();
        assert_eq!(serde_json::to_value(&s).unwrap(), json!({"type": "oneWaySynonym", "input": "phone", "synonyms": ["smartphone"]}));
    }
}
