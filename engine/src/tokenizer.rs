//! Text normalization and tokenization.
//!
//! The same normalization runs on indexed text and on queries, so "Élan",
//! "ELAN" and "elan" all become the token `elan`. Like Algolia, the index keeps
//! the original words; plurals, synonyms and typos are handled later, at query
//! time.
//!
//! Each token records its word position (for proximity ranking) and its byte
//! range in the original text (for highlighting).
//!
//! Chinese and Japanese are written without spaces, so runs of Han, Hiragana
//! and Katakana characters are split into words with a dictionary: Lindera
//! with IPADIC for Japanese, jieba for Chinese. Text containing kana is
//! Japanese; text with Han characters only is Chinese unless the index says
//! its language is Japanese (`indexLanguages: ["ja"]`), which a Japanese
//! index should, so that queries in kanji only are split the same way. With `parts`, the
//! dictionary words inside a longer word are added too ("清华大学" also gives
//! `清华` and `大学`), so a search for a part finds the whole word.

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
    /// A word inside the previous non-part token, at the same position
    /// (only with `Segmentation::parts`).
    pub part: bool,
}

/// Which language a run of Han characters (shared by Chinese and Japanese)
/// is split as.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HanLanguage {
    #[default]
    Chinese,
    Japanese,
}

impl HanLanguage {
    /// From Algolia's `indexLanguages`: Japanese when "ja" is listed and
    /// "zh" is not.
    pub fn from_languages(languages: &[String]) -> Self {
        let has = |code: &str| languages.iter().any(|l| l.eq_ignore_ascii_case(code));
        if has("ja") && !has("zh") { HanLanguage::Japanese } else { HanLanguage::Chinese }
    }
}

/// How text without spaces is split into words.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Segmentation {
    pub han: HanLanguage,
    /// Also emit the dictionary words inside longer words.
    pub parts: bool,
}

impl Segmentation {
    pub fn with_parts(self) -> Self {
        Segmentation { parts: true, ..self }
    }
}

/// Lowercases, removes accents and folds a few letters that Unicode does not
/// decompose (ß, æ, ø, ...). Compatibility forms are folded too: full-width
/// "Ｗｉｆｉ" is `wifi`, half-width "ﾊﾞｯｸﾞ" is `バッグ`. Japanese voicing marks
/// are kept (バッグ "bag" is not ハック "hack"). Non-word characters are kept
/// as-is; callers split on them.
pub fn normalize(text: &str) -> String {
    // Most words are plain ASCII: lowercasing is all there is to do.
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    let mut out = String::with_capacity(text.len());
    let mut voiced = false;
    for c in text.nfkd() {
        if is_voicing_mark(c) {
            voiced = true;
            out.push(c);
        } else {
            push_normalized(c, &mut out);
        }
    }
    // Put voiced kana back together: か + ゙ is が.
    if voiced { out.nfc().collect() } else { out }
}

/// The combining (dakuten) and handakuten marks of kana.
fn is_voicing_mark(c: char) -> bool {
    matches!(c, '\u{3099}' | '\u{309A}')
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

fn is_kana(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF   // Hiragana, Katakana
        | 0x31F0..=0x31FF // Katakana phonetic extensions
        | 0xFF66..=0xFF9F) // Half-width Katakana
}

/// Han, Hiragana and Katakana text has no spaces between words.
fn is_unspaced_script(c: char) -> bool {
    is_kana(c)
        || matches!(c as u32,
            0x3005..=0x3007   // 々 〆 〇
            | 0x3400..=0x4DBF // CJK Extension A
            | 0x4E00..=0x9FFF // CJK Unified Ideographs
            | 0xF900..=0xFAFF // CJK Compatibility Ideographs
            | 0x20000..=0x2FFFF)
}

/// Whether a (normalized) word is Chinese or Japanese. Such words get no
/// typo tolerance: one character more or less is a different word.
pub fn is_cjk_word(word: &str) -> bool {
    word.chars().any(is_unspaced_script)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || is_combining_mark(c)
}

/// Splits text into normalized tokens. Anything that is not a letter or digit
/// separates words, so "built-in" gives `built`, `in` and "5.3" gives `5`, `3`.
pub fn tokenize(text: &str) -> Vec<Token> {
    tokenize_with(text, Segmentation::default())
}

pub fn tokenize_with(text: &str, segmentation: Segmentation) -> Vec<Token> {
    let mut tokens = Vec::new();
    tokenize_into(text, 0, 0, segmentation, &mut tokens);
    tokens
}

/// Each token with the part tokens that follow it.
pub fn group_parts(tokens: Vec<Token>) -> Vec<(Token, Vec<Token>)> {
    let mut grouped: Vec<(Token, Vec<Token>)> = Vec::new();
    for token in tokens {
        match grouped.last_mut() {
            Some((_, parts)) if token.part => parts.push(token),
            _ => grouped.push((token, Vec::new())),
        }
    }
    grouped
}

/// Appends tokens of `text` starting at word position `first_position`.
/// Returns the next free position.
fn tokenize_into(text: &str, first_position: u32, element: u32, segmentation: Segmentation, tokens: &mut Vec<Token>) -> u32 {
    let mut position = first_position;
    let mut push = |start: usize, end: usize, part: bool, position: &mut u32| {
        let normalized = normalize(&text[start..end]);
        // A word made only of combining marks normalizes to nothing.
        if normalized.is_empty() {
            return;
        }
        if part {
            // Parts share the position of the word they are in.
            tokens.push(Token { text: normalized, position: *position - 1, element, start, end, part });
        } else {
            tokens.push(Token { text: normalized, position: *position, element, start, end, part });
            *position += 1;
        }
    };

    // Kana anywhere makes the Han words of this text Japanese too.
    let japanese = segmentation.han == HanLanguage::Japanese || text.chars().any(is_kana);
    let mut word_start = None;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if is_unspaced_script(c) {
            if let Some(start) = word_start.take() {
                push(start, i, false, &mut position);
            }
            let mut end = i + c.len_utf8();
            while let Some(&(j, d)) = chars.peek() {
                if !is_unspaced_script(d) {
                    break;
                }
                end = j + d.len_utf8();
                chars.next();
            }
            for word in segment(&text[i..end], japanese, segmentation.parts) {
                push(i + word.start, i + word.end, false, &mut position);
                for part in word.parts {
                    push(i + part.0, i + part.1, true, &mut position);
                }
            }
        } else if is_word_char(c) {
            word_start.get_or_insert(i);
        } else if let Some(start) = word_start.take() {
            push(start, i, false, &mut position);
        }
    }
    if let Some(start) = word_start {
        push(start, text.len(), false, &mut position);
    }
    position
}

/// A word found in a run of unspaced text: byte ranges within the run.
struct Segment {
    start: usize,
    end: usize,
    /// Dictionary words strictly inside it, in order.
    parts: Vec<(usize, usize)>,
}

/// Splits a run of Han and kana characters into words.
fn segment(run: &str, japanese: bool, parts: bool) -> Vec<Segment> {
    let (words, all) = if japanese { dictionary::japanese(run, parts) } else { dictionary::chinese(run, parts) };
    words
        .into_iter()
        .map(|(start, end)| {
            let mut parts: Vec<(usize, usize)> =
                all.iter().copied().filter(|&(s, e)| s >= start && e <= end && (s, e) != (start, end)).collect();
            parts.sort_unstable();
            parts.dedup();
            Segment { start, end, parts }
        })
        .collect()
}

/// The dictionary segmenters. Each returns the words of a run as byte
/// ranges, and (with `parts`) every dictionary word found, which may overlap.
/// Without the `japanese` or `chinese` feature, each character is a word.
mod dictionary {
    type Ranges = Vec<(usize, usize)>;

    #[allow(dead_code)]
    fn characters(run: &str) -> (Ranges, Ranges) {
        (run.char_indices().map(|(i, c)| (i, i + c.len_utf8())).collect(), Vec::new())
    }

    #[cfg(feature = "japanese")]
    pub fn japanese(run: &str, parts: bool) -> (Ranges, Ranges) {
        use lindera::dictionary::load_dictionary;
        use lindera::mode::{Mode, Penalty};
        use lindera::segmenter::Segmenter;
        use std::sync::OnceLock;

        static NORMAL: OnceLock<Segmenter> = OnceLock::new();
        static DECOMPOSE: OnceLock<Segmenter> = OnceLock::new();
        let load = |mode| {
            let dictionary = load_dictionary("embedded://ipadic").expect("the IPADIC dictionary is embedded");
            Segmenter::new(mode, dictionary, None)
        };
        let ranges = |segmenter: &Segmenter| -> Ranges {
            match segmenter.segment(run.into()) {
                Ok(tokens) => tokens.iter().map(|t| (t.byte_start, t.byte_end)).collect(),
                Err(_) => characters(run).0,
            }
        };
        let words = ranges(NORMAL.get_or_init(|| load(Mode::Normal)));
        let all = if parts { ranges(DECOMPOSE.get_or_init(|| load(Mode::Decompose(Penalty::default())))) } else { Vec::new() };
        (words, all)
    }

    #[cfg(not(feature = "japanese"))]
    pub fn japanese(run: &str, _parts: bool) -> (Ranges, Ranges) {
        characters(run)
    }

    #[cfg(feature = "chinese")]
    pub fn chinese(run: &str, parts: bool) -> (Ranges, Ranges) {
        use jieba_rs::Jieba;
        use std::sync::OnceLock;

        static JIEBA: OnceLock<Jieba> = OnceLock::new();
        let jieba = JIEBA.get_or_init(Jieba::new);
        // Without the HMM, words missing from the dictionary become single
        // characters, the same way in records and in queries.
        let words = jieba.cut(run, false).iter().map(|t| (t.byte_start, t.byte_end)).collect();
        let all = if parts { jieba.cut_for_search(run, false).iter().map(|t| (t.byte_start, t.byte_end)).collect() } else { Vec::new() };
        (words, all)
    }

    #[cfg(not(feature = "chinese"))]
    pub fn chinese(run: &str, _parts: bool) -> (Ranges, Ranges) {
        characters(run)
    }
}

/// Tokenizes one attribute value. Strings and numbers are indexed; arrays are
/// flattened with a position gap between elements; objects, booleans and
/// nulls are not searchable.
pub fn tokenize_value(value: &Value, segmentation: Segmentation) -> Vec<Token> {
    let mut tokens = Vec::new();
    match value {
        Value::String(s) => {
            tokenize_into(s, 0, 0, segmentation, &mut tokens);
        }
        Value::Number(n) => {
            tokenize_into(&n.to_string(), 0, 0, segmentation, &mut tokens);
        }
        Value::Array(items) => {
            let mut position = 0;
            for (element, item) in items.iter().enumerate() {
                let text = match item {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => continue,
                };
                let next = tokenize_into(&text, position, element as u32, segmentation, &mut tokens);
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
    fn ascii_fast_path_matches_full_normalization() {
        let full = |text: &str| {
            let mut out = String::new();
            for c in text.nfd() {
                push_normalized(c, &mut out);
            }
            out
        };
        for text in ["Kettle", "USB-C 5.3", "ABC xyz 019", "", "  MiXeD  ", "a_b~c!"] {
            assert_eq!(normalize(text), full(text), "{text:?}");
        }
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

    fn tokenize_value_default(value: &Value) -> Vec<Token> {
        tokenize_value(value, Segmentation::default())
    }

    #[allow(dead_code)] // unused without the dictionary features
    fn ja() -> Segmentation {
        Segmentation { han: HanLanguage::Japanese, parts: false }
    }

    /// (text, position, is part)
    #[allow(dead_code)]
    fn layout(tokens: &[Token]) -> Vec<(&str, u32, bool)> {
        tokens.iter().map(|t| (t.text.as_str(), t.position, t.part)).collect()
    }

    #[test]
    fn japanese_voicing_and_width_are_normalized() {
        assert_eq!(normalize("バッグ"), "バッグ");
        assert_ne!(normalize("バッグ"), normalize("ハック"));
        assert_eq!(normalize("ﾊﾞｯｸﾞ"), "バッグ");
        assert_eq!(normalize("Ｗｉｆｉ６"), "wifi6");
        // Decomposed input gives the same composed result.
        assert_eq!(normalize("ハ\u{3099}ック\u{3099}"), "バッグ");
    }

    #[test]
    fn languages_choose_how_han_text_is_split() {
        assert_eq!(HanLanguage::from_languages(&[]), HanLanguage::Chinese);
        assert_eq!(HanLanguage::from_languages(&["ja".into()]), HanLanguage::Japanese);
        assert_eq!(HanLanguage::from_languages(&["JA".into(), "en".into()]), HanLanguage::Japanese);
        assert_eq!(HanLanguage::from_languages(&["ja".into(), "zh".into()]), HanLanguage::Chinese);
        assert!(is_cjk_word("東京") && is_cjk_word("タワー") && is_cjk_word("tシャツ"));
        assert!(!is_cjk_word("tokyo") && !is_cjk_word("élan"));
    }

    #[cfg(feature = "japanese")]
    #[test]
    fn japanese_is_split_into_dictionary_words() {
        let text = "東京タワーに行きました tokyo";
        let tokens = tokenize(text);
        assert_eq!(texts(&tokens), ["東京", "タワー", "に", "行き", "まし", "た", "tokyo"]);
        assert_eq!(tokens.iter().map(|t| t.position).collect::<Vec<_>>(), [0, 1, 2, 3, 4, 5, 6]);
        let originals: Vec<&str> = tokens.iter().map(|t| &text[t.start..t.end]).collect();
        assert_eq!(originals[..2], ["東京", "タワー"]);
        // Mixed with Latin letters and digits.
        assert_eq!(texts(&tokenize("ワイヤレスイヤホン Bluetooth5.3対応")), ["ワイヤレスイヤホン", "bluetooth5", "3", "対応"]);
        // Kanji only: Japanese when the index says so.
        assert_eq!(texts(&tokenize_with("東京都", ja())), ["東京", "都"]);
    }

    #[cfg(feature = "japanese")]
    #[test]
    fn japanese_compounds_also_give_their_parts() {
        let tokens = tokenize_with("関西国際空港限定", ja().with_parts());
        assert_eq!(
            layout(&tokens),
            [("関西国際空港", 0, false), ("関西", 0, true), ("国際", 0, true), ("空港", 0, true), ("限定", 1, false)]
        );
        let grouped = group_parts(tokens);
        assert_eq!(grouped.len(), 2);
        assert_eq!(texts(&grouped[0].1), ["関西", "国際", "空港"]);
    }

    #[cfg(feature = "chinese")]
    #[test]
    fn chinese_is_split_into_dictionary_words() {
        assert_eq!(texts(&tokenize("无线蓝牙耳机降噪")), ["无线", "蓝牙", "耳机", "降噪"]);
        assert_eq!(texts(&tokenize("我来到北京清华大学")), ["我", "来到", "北京", "清华大学"]);
        let parts = Segmentation::default().with_parts();
        assert_eq!(
            layout(&tokenize_with("清华大学 tsinghua", parts)),
            [("清华大学", 0, false), ("清华", 0, true), ("华大", 0, true), ("大学", 0, true), ("tsinghua", 1, false)]
        );
    }

    #[cfg(not(feature = "chinese"))]
    #[test]
    fn without_a_dictionary_each_character_is_a_word() {
        assert_eq!(texts(&tokenize("北京大学")), ["北", "京", "大", "学"]);
    }

    #[test]
    fn empty_and_separator_only_text_has_no_tokens() {
        assert!(tokenize("").is_empty());
        assert!(tokenize(" -- , ... ").is_empty());
    }

    #[test]
    fn arrays_get_a_gap_between_elements() {
        let tokens = tokenize_value_default(&json!(["fast charging", true, "USB-C"]));
        assert_eq!(texts(&tokens), ["fast", "charging", "usb", "c"]);
        let positions: Vec<u32> = tokens.iter().map(|t| t.position).collect();
        assert_eq!(positions, [0, 1, 2 + ELEMENT_GAP, 3 + ELEMENT_GAP]);
        let elements: Vec<u32> = tokens.iter().map(|t| t.element).collect();
        assert_eq!(elements, [0, 0, 2, 2]);
    }

    #[test]
    fn numbers_are_searchable_but_bools_and_objects_are_not() {
        assert_eq!(texts(&tokenize_value_default(&json!(1996))), ["1996"]);
        assert!(tokenize_value_default(&json!(true)).is_empty());
        assert!(tokenize_value_default(&json!({"a": "b"})).is_empty());
        assert!(tokenize_value_default(&json!(null)).is_empty());
    }

    #[test]
    fn handles_every_generated_dataset_without_empty_tokens() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../data");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // datagen has not been run; nothing to check
        };
        for entry in entries.flatten() {
            if entry.path().extension().is_none_or(|e| e != "jsonl") {
                continue; // e.g. the data/scale directory
            }
            let text = std::fs::read_to_string(entry.path()).unwrap();
            for line in text.lines().take(500) {
                let record: Value = serde_json::from_str(line).unwrap();
                for value in record.as_object().unwrap().values() {
                    for t in tokenize_value(value, Segmentation::default()) {
                        assert!(!t.text.is_empty());
                        assert!(t.text.chars().all(|c| c.is_alphanumeric()), "{:?}", t.text);
                    }
                }
            }
        }
    }
}
