//! Text normalization and tokenization.
//!
//! The same normalization runs on indexed text and on queries, so "Élan",
//! "ELAN" and "elan" all become the token `elan`. Like Algolia, the index keeps
//! the original words; plurals, synonyms and typos are handled later, at query
//! time.
//!
//! Each token records its word position (for proximity ranking) and its byte
//! range in the original text (for highlighting).

use serde_json::Value;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Position gap inserted between array elements, so words from different
/// elements never look adjacent. It equals the proximity cap used in ranking.
pub const ELEMENT_GAP: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Normalized form, e.g. `elan`.
    pub text: String,
    /// Word position within the attribute, counting from 0.
    pub position: u32,
    /// Array element the token came from (0 for plain strings).
    pub element: u32,
    /// Byte range of the original word inside its element's text.
    pub start: usize,
    pub end: usize,
}

/// Lowercases, removes accents and folds a few letters that Unicode does not
/// decompose (ß, æ, ø, ...). Non-word characters are kept as-is; callers
/// split on them.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.nfd() {
        push_normalized(c, &mut out);
    }
    out
}

fn push_normalized(c: char, out: &mut String) {
    if is_combining_mark(c) {
        return;
    }
    match c {
        'ß' | 'ẞ' => out.push_str("ss"),
        'æ' | 'Æ' => out.push_str("ae"),
        'œ' | 'Œ' => out.push_str("oe"),
        'ø' | 'Ø' => out.push('o'),
        'đ' | 'Đ' => out.push('d'),
        'ł' | 'Ł' => out.push('l'),
        'þ' | 'Þ' => out.push_str("th"),
        'ı' => out.push('i'),
        _ => out.extend(c.to_lowercase()),
    }
}

/// Han, Hiragana and Katakana text has no spaces between words. Until we add
/// a dictionary segmenter, each of these characters is its own token.
fn is_unspaced_script(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF   // Hiragana, Katakana
        | 0x3400..=0x4DBF // CJK Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xF900..=0xFAFF // CJK Compatibility Ideographs
        | 0x20000..=0x2FFFF)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || is_combining_mark(c)
}

/// Splits text into normalized tokens. Anything that is not a letter or digit
/// separates words, so "built-in" gives `built`, `in` and "5.3" gives `5`, `3`.
pub fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    tokenize_into(text, 0, 0, &mut tokens);
    tokens
}

/// Appends tokens of `text` starting at word position `first_position`.
/// Returns the next free position.
fn tokenize_into(text: &str, first_position: u32, element: u32, tokens: &mut Vec<Token>) -> u32 {
    let mut position = first_position;
    let mut word = String::new();
    let mut start = 0;

    let mut flush = |word: &mut String, start: usize, end: usize, position: &mut u32| {
        if word.is_empty() {
            return;
        }
        let normalized = normalize(word);
        word.clear();
        // A word made only of combining marks normalizes to nothing.
        if normalized.is_empty() {
            return;
        }
        tokens.push(Token { text: normalized, position: *position, element, start, end });
        *position += 1;
    };

    for (i, c) in text.char_indices() {
        if is_unspaced_script(c) {
            flush(&mut word, start, i, &mut position);
            word.push(c);
            flush(&mut word, i, i + c.len_utf8(), &mut position);
        } else if is_word_char(c) {
            if word.is_empty() {
                start = i;
            }
            word.push(c);
        } else {
            flush(&mut word, start, i, &mut position);
        }
    }
    flush(&mut word, start, text.len(), &mut position);
    position
}

/// Tokenizes one attribute value. Strings and numbers are indexed; arrays are
/// flattened with a position gap between elements; objects, booleans and
/// nulls are not searchable.
pub fn tokenize_value(value: &Value) -> Vec<Token> {
    let mut tokens = Vec::new();
    match value {
        Value::String(s) => {
            tokenize_into(s, 0, 0, &mut tokens);
        }
        Value::Number(n) => {
            tokenize_into(&n.to_string(), 0, 0, &mut tokens);
        }
        Value::Array(items) => {
            let mut position = 0;
            for (element, item) in items.iter().enumerate() {
                let text = match item {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => continue,
                };
                let next = tokenize_into(&text, position, element as u32, &mut tokens);
                if next > position {
                    position = next + ELEMENT_GAP;
                }
            }
        }
        Value::Object(_) | Value::Bool(_) | Value::Null => {}
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn texts(tokens: &[Token]) -> Vec<&str> {
        tokens.iter().map(|t| t.text.as_str()).collect()
    }

    #[test]
    fn normalizes_case_accents_and_special_letters() {
        assert_eq!(normalize("Élan Brücke Chloé GARCÍA"), "elan brucke chloe garcia");
        assert_eq!(normalize("Straße Æsir Øre Łódź"), "strasse aesir ore lodz");
        // Already-decomposed input (e + combining acute) gives the same result.
        assert_eq!(normalize("Cafe\u{301}"), "cafe");
    }

    #[test]
    fn splits_on_punctuation_and_records_positions() {
        let tokens = tokenize("Built-in  microphone, bluetooth 5.3!");
        assert_eq!(texts(&tokens), ["built", "in", "microphone", "bluetooth", "5", "3"]);
        assert_eq!(tokens.iter().map(|t| t.position).collect::<Vec<_>>(), [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn byte_offsets_point_at_original_words() {
        let text = "The Élan Kettle";
        let tokens = tokenize(text);
        let originals: Vec<&str> = tokens.iter().map(|t| &text[t.start..t.end]).collect();
        assert_eq!(originals, ["The", "Élan", "Kettle"]);
        assert_eq!(tokens[1].text, "elan");
    }

    #[test]
    fn decomposed_accents_stay_inside_the_word() {
        let text = "cafe\u{301} noir";
        let tokens = tokenize(text);
        assert_eq!(texts(&tokens), ["cafe", "noir"]);
        assert_eq!(&text[tokens[0].start..tokens[0].end], "cafe\u{301}");
    }

    #[test]
    fn unspaced_scripts_become_single_characters() {
        let tokens = tokenize("東京タワー tokyo");
        assert_eq!(texts(&tokens), ["東", "京", "タ", "ワ", "ー", "tokyo"]);
        assert_eq!(tokens[5].position, 5);
    }

    #[test]
    fn empty_and_separator_only_text_has_no_tokens() {
        assert!(tokenize("").is_empty());
        assert!(tokenize(" -- , ... ").is_empty());
    }

    #[test]
    fn arrays_get_a_gap_between_elements() {
        let tokens = tokenize_value(&json!(["fast charging", true, "USB-C"]));
        assert_eq!(texts(&tokens), ["fast", "charging", "usb", "c"]);
        let positions: Vec<u32> = tokens.iter().map(|t| t.position).collect();
        assert_eq!(positions, [0, 1, 2 + ELEMENT_GAP, 3 + ELEMENT_GAP]);
        let elements: Vec<u32> = tokens.iter().map(|t| t.element).collect();
        assert_eq!(elements, [0, 0, 2, 2]);
    }

    #[test]
    fn numbers_are_searchable_but_bools_and_objects_are_not() {
        assert_eq!(texts(&tokenize_value(&json!(1996))), ["1996"]);
        assert!(tokenize_value(&json!(true)).is_empty());
        assert!(tokenize_value(&json!({"a": "b"})).is_empty());
        assert!(tokenize_value(&json!(null)).is_empty());
    }

    #[test]
    fn handles_every_generated_dataset_without_empty_tokens() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../data");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // datagen has not been run; nothing to check
        };
        for entry in entries.flatten() {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            for line in text.lines().take(500) {
                let record: Value = serde_json::from_str(line).unwrap();
                for value in record.as_object().unwrap().values() {
                    for t in tokenize_value(value) {
                        assert!(!t.text.is_empty());
                        assert!(t.text.chars().all(|c| c.is_alphanumeric()), "{:?}", t.text);
                    }
                }
            }
        }
    }
}
