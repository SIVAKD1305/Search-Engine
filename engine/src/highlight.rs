//! Highlighting and snippets, in Algolia's `_highlightResult` and
//! `_snippetResult` format.
//!
//! Only the hits on the returned page are processed: their attributes are
//! re-tokenized (keeping byte offsets into the original text) and each token
//! is looked up among the words the query matched, including typo and prefix
//! matches. A prefix match marks only the typed part: "head" gives
//! `<em>Head</em>phones`.
//!
//! The record text is HTML-escaped; only the highlight tags are raw HTML.

use crate::search::QueryTerm;
use crate::tokenizer::{Token, normalize, tokenize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap};

/// Words shown in a snippet when `attributesToSnippet` gives no count.
pub const DEFAULT_SNIPPET_WORDS: usize = 10;

#[derive(Debug, Clone)]
pub struct HighlightTags {
    pub pre: String,
    pub post: String,
    pub ellipsis: String,
}

impl Default for HighlightTags {
    fn default() -> Self {
        HighlightTags { pre: "<em>".into(), post: "</em>".into(), ellipsis: "…".into() }
    }
}

/// What a dictionary word matched: the query term and, for prefix matches,
/// how many normalized characters to mark.
#[derive(Debug, Clone, Copy)]
struct WordHit {
    term: usize,
    marked_chars: Option<usize>,
}

/// Looks up tokens among the query's matched words. Built once per search.
pub struct Matcher<'a> {
    words: HashMap<&'a str, Vec<WordHit>>,
    /// (prefix, hit) for prefix groups: a token starting with the prefix matches.
    prefixes: Vec<(&'a str, WordHit)>,
    terms: Vec<&'a str>,
}

impl<'a> Matcher<'a> {
    pub fn new(terms: &'a [QueryTerm]) -> Self {
        let mut words: HashMap<&str, Vec<WordHit>> = HashMap::new();
        let mut prefixes = Vec::new();
        for (term, t) in terms.iter().enumerate() {
            let typed = t.text.chars().count();
            for m in &t.matches {
                let marked_chars = m.prefix.then_some(typed);
                words.entry(m.word.as_str()).or_default().push(WordHit { term, marked_chars });
            }
            for p in &t.prefixes {
                prefixes.push((p.prefix.as_str(), WordHit { term, marked_chars: Some(typed) }));
            }
        }
        Matcher { words, prefixes, terms: terms.iter().map(|t| t.text.as_str()).collect() }
    }

    fn lookup(&self, token: &Token) -> Vec<WordHit> {
        let mut hits = self.words.get(token.text.as_str()).cloned().unwrap_or_default();
        hits.extend(self.prefixes.iter().filter(|(p, _)| token.text.starts_with(p)).map(|(_, h)| *h));
        hits
    }
}

/// A token's match: the bytes to mark at its start, and the terms it matched.
struct Mark {
    marked_bytes: usize,
    terms: Vec<usize>,
}

fn mark_for(original: &str, token: &Token, matcher: &Matcher) -> Option<Mark> {
    let hits = matcher.lookup(token);
    if hits.is_empty() {
        return None;
    }
    let whole = token.end - token.start;
    let word = &original[token.start..token.end];
    // A whole-word match marks everything; otherwise mark the longest prefix.
    let marked_bytes = hits
        .iter()
        .map(|h| h.marked_chars.map_or(whole, |n| original_prefix_bytes(word, n)))
        .max()
        .unwrap_or(whole);
    Some(Mark { marked_bytes, terms: hits.iter().map(|h| h.term).collect() })
}

/// Byte length of the shortest start of `original` whose normalized form has
/// at least `chars` characters, so "Élan" with 2 gives "Él".
fn original_prefix_bytes(original: &str, chars: usize) -> usize {
    let mut produced = 0;
    let mut end = 0;
    for (i, c) in original.char_indices() {
        let n = normalize(c.encode_utf8(&mut [0; 4])).chars().count();
        if produced >= chars && n > 0 {
            break;
        }
        produced += n;
        end = i + c.len_utf8();
    }
    end
}

fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

/// Renders `text[from..to]`, wrapping the marked part of matched tokens.
fn render(text: &str, tokens: &[(Token, Option<Mark>)], from: usize, to: usize, tags: &HighlightTags) -> String {
    let mut out = String::with_capacity(to - from + 16);
    let mut cursor = from;
    for (token, mark) in tokens {
        let Some(mark) = mark else { continue };
        if token.start < from || token.end > to {
            continue;
        }
        escape(&text[cursor..token.start], &mut out);
        out.push_str(&tags.pre);
        escape(&text[token.start..token.start + mark.marked_bytes], &mut out);
        out.push_str(&tags.post);
        cursor = token.start + mark.marked_bytes;
    }
    escape(&text[cursor..to], &mut out);
    out
}

fn match_level(matched: &BTreeSet<usize>, n_terms: usize) -> &'static str {
    if matched.is_empty() {
        "none"
    } else if matched.len() == n_terms {
        "full"
    } else {
        "partial"
    }
}

fn analyse(text: &str, matcher: &Matcher) -> Vec<(Token, Option<Mark>)> {
    tokenize(text)
        .into_iter()
        .map(|t| {
            let mark = mark_for(text, &t, matcher);
            (t, mark)
        })
        .collect()
}

fn highlight_text(text: &str, matcher: &Matcher, tags: &HighlightTags) -> Value {
    let tokens = analyse(text, matcher);
    let matched: BTreeSet<usize> = tokens.iter().flat_map(|(_, m)| m.iter().flat_map(|m| m.terms.iter().copied())).collect();
    let mut result = json!({
        "value": render(text, &tokens, 0, text.len(), tags),
        "matchLevel": match_level(&matched, matcher.terms.len()),
        "matchedWords": matched.iter().map(|&t| matcher.terms[t]).collect::<Vec<_>>(),
    });
    if !matched.is_empty() {
        let fully = tokens.iter().all(|(t, m)| m.as_ref().is_some_and(|m| m.marked_bytes == t.end - t.start));
        result["fullyHighlighted"] = Value::Bool(fully);
    }
    result
}

/// The `words`-token window with the most distinct query terms (then the most
/// matched tokens, then the earliest), with ellipses where text was cut.
fn snippet_text(text: &str, words: usize, matcher: &Matcher, tags: &HighlightTags) -> Value {
    let tokens = analyse(text, matcher);
    let words = words.max(1);
    let all_matched: BTreeSet<usize> =
        tokens.iter().flat_map(|(_, m)| m.iter().flat_map(|m| m.terms.iter().copied())).collect();

    // Score: (distinct terms, matched tokens, -imbalance); ties keep the earliest.
    let mut best: Option<((usize, usize, isize), usize)> = None;
    for start in 0..tokens.len().saturating_sub(words - 1).max(1) {
        let window = &tokens[start..(start + words).min(tokens.len())];
        let distinct: BTreeSet<usize> =
            window.iter().flat_map(|(_, m)| m.iter().flat_map(|m| m.terms.iter().copied())).collect();
        let matched: Vec<usize> = window.iter().enumerate().filter(|(_, (_, m))| m.is_some()).map(|(i, _)| i).collect();
        // Prefer windows where the matches sit in the middle.
        let imbalance = match (matched.first(), matched.last()) {
            (Some(&first), Some(&last)) => (first as isize - (window.len() - 1 - last) as isize).abs(),
            _ => 0,
        };
        let score = (distinct.len(), matched.len(), -imbalance);
        if best.is_none_or(|(b, _)| score > b) {
            best = Some((score, start));
        }
    }
    let start = best.map_or(0, |(_, start)| start);

    let value = if tokens.is_empty() {
        let mut out = String::new();
        escape(text, &mut out);
        out
    } else {
        let end = (start + words).min(tokens.len());
        let (from, to) = (tokens[start].0.start, tokens[end - 1].0.end);
        let mut value = String::new();
        if start > 0 {
            value.push_str(&tags.ellipsis);
            value.push(' ');
        }
        value.push_str(&render(text, &tokens, from, to, tags));
        if end < tokens.len() {
            value.push(' ');
            value.push_str(&tags.ellipsis);
        }
        value
    };
    json!({ "value": value, "matchLevel": match_level(&all_matched, matcher.terms.len()) })
}

/// Text of a highlightable value; numbers are shown as written.
fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Applies `f` to a string, or to each element of an array.
fn per_value(value: &Value, f: impl Fn(&str) -> Value) -> Option<Value> {
    match value {
        Value::Array(items) => Some(Value::Array(items.iter().filter_map(as_text).map(|t| f(&t)).collect())),
        v => as_text(v).map(|t| f(&t)),
    }
}

/// The highlight of a text known to contain no matched word.
fn unmatched_text(text: &str) -> Value {
    let mut value = String::with_capacity(text.len());
    escape(text, &mut value);
    json!({ "value": value, "matchLevel": "none", "matchedWords": [] })
}

/// `_highlightResult` for the given attributes of one record.
/// `may_match(attribute)` returns false for attributes known to contain no
/// matched word; those are escaped without being tokenized.
pub fn highlight(
    record: &Map<String, Value>,
    attributes: &[String],
    matcher: &Matcher,
    tags: &HighlightTags,
    may_match: impl Fn(&str) -> bool,
) -> Map<String, Value> {
    attributes
        .iter()
        .filter_map(|a| {
            let value = record.get(a)?;
            let highlighted = if may_match(a) {
                per_value(value, |t| highlight_text(t, matcher, tags))
            } else {
                per_value(value, unmatched_text)
            };
            highlighted.map(|v| (a.clone(), v))
        })
        .collect()
}

/// `_snippetResult`; each spec is `attribute` or `attribute:words`.
pub fn snippet(record: &Map<String, Value>, specs: &[String], matcher: &Matcher, tags: &HighlightTags) -> Map<String, Value> {
    specs
        .iter()
        .filter_map(|spec| {
            let (attribute, words) = match spec.rsplit_once(':') {
                Some((a, n)) => (a, n.trim().parse().unwrap_or(DEFAULT_SNIPPET_WORDS)),
                None => (spec.as_str(), DEFAULT_SNIPPET_WORDS),
            };
            let value = record.get(attribute)?;
            per_value(value, |t| snippet_text(t, words, matcher, tags)).map(|v| (attribute.to_owned(), v))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Index, Settings};
    use crate::search::plan;
    use serde_json::json;

    fn index() -> Index {
        let mut index = Index::new(Settings::default());
        index.add(json!({"objectID": "1", "name": "Élan Wireless Headphones"})).unwrap();
        index.add(json!({"objectID": "2", "name": "Café <Noir> & co"})).unwrap();
        index
    }

    fn hl(index: &Index, query: &str, text: &str) -> Value {
        let terms = plan(index, query);
        highlight_text(text, &Matcher::new(&terms), &HighlightTags::default())
    }

    #[test]
    fn marks_whole_words_typos_and_only_the_typed_prefix() {
        let index = index();
        let r = hl(&index, "wirless head", "Élan Wireless Headphones");
        assert_eq!(r["value"], "Élan <em>Wireless</em> <em>Head</em>phones");
        assert_eq!(r["matchLevel"], "full");
        assert_eq!(r["matchedWords"], json!(["wirless", "head"]));
        assert_eq!(r["fullyHighlighted"], false);
    }

    #[test]
    fn prefix_marks_respect_accents() {
        let index = index();
        assert_eq!(hl(&index, "el", "Élan Wireless")["value"], "<em>Él</em>an Wireless");
        assert_eq!(hl(&index, "elan", "ÉLAN")["value"], "<em>ÉLAN</em>");
        assert_eq!(hl(&index, "elan", "ÉLAN")["fullyHighlighted"], true);
    }

    #[test]
    fn partial_and_no_match_levels() {
        let index = index();
        let r = hl(&index, "elan headphones", "Élan Kettle");
        assert_eq!((r["matchLevel"].as_str(), r["matchedWords"].clone()), (Some("partial"), json!(["elan"])));
        let r = hl(&index, "elan", "Kettle");
        assert_eq!(r["matchLevel"], "none");
        assert!(r.get("fullyHighlighted").is_none());
    }

    #[test]
    fn record_text_is_escaped_but_tags_are_not() {
        let index = index();
        assert_eq!(hl(&index, "noir", "Café <Noir> & co")["value"], "Café &lt;<em>Noir</em>&gt; &amp; co");
    }

    #[test]
    fn original_prefix_bytes_handles_folding_and_combining_marks() {
        assert_eq!(original_prefix_bytes("Élan", 2), "Él".len());
        assert_eq!(original_prefix_bytes("Straße", 6), "Straß".len());
        assert_eq!(original_prefix_bytes("cafe\u{301}s", 4), "cafe\u{301}".len());
        assert_eq!(original_prefix_bytes("abc", 10), 3);
    }

    #[test]
    fn snippet_picks_the_window_with_most_terms() {
        let mut index = Index::new(Settings::default());
        let body = "Intro words here. Search engines need an inverted index. Later we discuss Raft and \
                    consensus for replication, then raft again at the very end of the text.";
        index.add(json!({"objectID": "1", "body": body})).unwrap();
        let terms = plan(&index, "raft consensus");
        let m = Matcher::new(&terms);
        let s = snippet_text(body, 5, &m, &HighlightTags::default());
        assert_eq!(s["value"], "… discuss <em>Raft</em> and <em>consensus</em> for …");
        assert_eq!(s["matchLevel"], "full");
        let whole = snippet_text("Raft rules", 10, &m, &HighlightTags::default());
        assert_eq!(whole["value"], "<em>Raft</em> rules");
    }

    #[test]
    fn snippet_without_match_shows_the_start() {
        let index = index();
        let terms = plan(&index, "zzz");
        let s = snippet_text("one two three four", 2, &Matcher::new(&terms), &HighlightTags::default());
        assert_eq!(s["value"], "one two …");
        assert_eq!(s["matchLevel"], "none");
    }

    #[test]
    fn arrays_and_custom_tags() {
        let mut index = Index::new(Settings::default());
        index.add(json!({"objectID": "1", "features": ["fast charging", "USB-C"], "year": 1996})).unwrap();
        let terms = plan(&index, "usb 1996");
        let tags = HighlightTags { pre: "[".into(), post: "]".into(), ellipsis: "...".into() };
        let record = index.store().get(0).unwrap();
        let r = highlight(&record, &["features".into(), "year".into(), "missing".into()], &Matcher::new(&terms), &tags, |_| true);
        assert_eq!(r["features"][0]["value"], "fast charging");
        assert_eq!(r["features"][1]["value"], "[USB]-C");
        assert_eq!(r["year"]["value"], "[1996]");
        assert!(!r.contains_key("missing"));
    }
}
