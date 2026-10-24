//! Synonyms, in Algolia's record format:
//!
//! ```json
//! {"type": "synonym", "synonyms": ["tv", "television"]}
//! {"type": "oneWaySynonym", "input": "phone", "synonyms": ["smartphone"]}
//! ```
//!
//! A regular synonym makes every word stand for every other one; a one-way
//! synonym only expands `input`. Only single words are supported for now.

use crate::tokenizer::{Segmentation, tokenize_with};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

/// Normalizes a synonym word; it must be exactly one word. Chinese and
/// Japanese synonyms are split into words like any text, so "笔记本电脑"
/// (two words) needs multi-word synonyms.
fn one_word(word: &str, segmentation: Segmentation) -> Result<String, String> {
    let tokens = tokenize_with(word, segmentation);
    match tokens.as_slice() {
        [token] => Ok(token.text.clone()),
        [] => Err(format!("empty synonym {word:?}")),
        _ => Err(format!("multi-word synonyms are not supported yet: {word:?}")),
    }
}

impl TryFrom<RawSynonym> for Synonym {
    type Error = String;

    fn try_from(raw: RawSynonym) -> Result<Self, Self::Error> {
        let check = |words: &[String], min: usize| -> Result<(), String> {
            if words.len() < min {
                return Err(format!("a synonym needs at least {min} word(s)"));
            }
            words.iter().try_for_each(|w| one_word(w, Segmentation::default()).map(|_| ()))
        };
        match raw {
            RawSynonym::Synonym { synonyms } => {
                check(&synonyms, 2)?;
                Ok(Synonym::Synonym { synonyms })
            }
            RawSynonym::OneWaySynonym { input, synonyms } => {
                one_word(&input, Segmentation::default())?;
                check(&synonyms, 1)?;
                Ok(Synonym::OneWaySynonym { input, synonyms })
            }
        }
    }
}

/// Normalized word -> the words it can stand for.
pub fn build_map(synonyms: &[Synonym], segmentation: Segmentation) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let normalized =
        |words: &[String]| -> Vec<String> { words.iter().filter_map(|w| one_word(w, segmentation).ok()).collect() };
    for synonym in synonyms {
        match synonym {
            Synonym::Synonym { synonyms } => {
                let words = normalized(synonyms);
                for from in &words {
                    let targets = map.entry(from.clone()).or_default();
                    targets.extend(words.iter().filter(|w| *w != from).cloned());
                }
            }
            Synonym::OneWaySynonym { input, synonyms } => {
                if let Ok(from) = one_word(input, segmentation) {
                    map.entry(from.clone()).or_default().extend(normalized(synonyms).into_iter().filter(|w| *w != from));
                }
            }
        }
    }
    for targets in map.values_mut() {
        targets.sort();
        targets.dedup();
    }
    map
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
        assert_eq!(map["tv"], ["television", "telly"]);
        assert_eq!(map["television"], ["telly", "tv"]);
        assert_eq!(map["phone"], ["smartphone"]);
        assert!(!map.contains_key("smartphone"));
    }

    #[test]
    fn rejects_invalid_synonyms() {
        assert!(parse(json!({"type": "synonym", "synonyms": ["tv"]})).unwrap_err().contains("at least 2"));
        assert!(parse(json!({"type": "synonym", "synonyms": ["new york", "ny"]})).unwrap_err().contains("multi-word"));
        assert!(parse(json!({"type": "oneWaySynonym", "input": "", "synonyms": ["x"]})).unwrap_err().contains("empty"));
        assert!(parse(json!({"type": "placeholder"})).is_err());
    }

    #[test]
    fn serializes_back_to_algolia_format() {
        let s = parse(json!({"type": "oneWaySynonym", "input": "phone", "synonyms": ["smartphone"]})).unwrap();
        assert_eq!(serde_json::to_value(&s).unwrap(), json!({"type": "oneWaySynonym", "input": "phone", "synonyms": ["smartphone"]}));
    }
}
