//! English plurals and stop words, used at query time.
//!
//! Like Algolia, nothing here changes the index: a query word is expanded to
//! its other forms, and only forms that exist in the dictionary are kept.

/// Irregular singular/plural pairs.
const IRREGULAR: &[(&str, &str)] = &[
    ("child", "children"),
    ("foot", "feet"),
    ("goose", "geese"),
    ("knife", "knives"),
    ("leaf", "leaves"),
    ("life", "lives"),
    ("man", "men"),
    ("mouse", "mice"),
    ("person", "people"),
    ("shelf", "shelves"),
    ("tooth", "teeth"),
    ("wife", "wives"),
    ("woman", "women"),
];

/// Common English words that rarely help find a record.
pub const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from", "has", "have", "i", "if", "in",
    "into", "is", "it", "its", "me", "my", "no", "not", "of", "on", "or", "our", "so", "such", "that", "the",
    "their", "then", "there", "these", "they", "this", "to", "was", "we", "will", "with", "you", "your",
];

pub fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

/// Other singular/plural forms of a normalized English word. The forms are
/// guesses ("shoes" gives both "shoe" and "sho"); callers keep the ones that
/// exist in the dictionary.
pub fn plural_forms(word: &str) -> Vec<String> {
    if word.chars().count() < 3 || !word.chars().all(|c| c.is_ascii_lowercase()) {
        return Vec::new();
    }
    let mut forms = Vec::new();
    for (singular, plural) in IRREGULAR {
        if word == *singular {
            forms.push(plural.to_string());
        } else if word == *plural {
            forms.push(singular.to_string());
        }
    }

    // Towards the singular.
    if let Some(stem) = word.strip_suffix("ies").filter(|s| s.len() >= 2) {
        forms.push(format!("{stem}y"));
    }
    if let Some(stem) = word.strip_suffix("es").filter(|s| s.len() >= 2) {
        forms.push(stem.to_owned());
    }
    if let Some(stem) = word.strip_suffix('s').filter(|s| !s.ends_with('s') && s.len() >= 2) {
        forms.push(stem.to_owned());
    }

    // Towards the plural.
    if !word.ends_with('s') || word.ends_with("ss") {
        let before_last = word.chars().rev().nth(1).unwrap_or('a');
        if let Some(stem) = word.strip_suffix('y').filter(|_| !is_vowel(before_last)) {
            forms.push(format!("{stem}ies"));
        } else if ["s", "x", "z", "ch", "sh"].iter().any(|end| word.ends_with(end)) {
            forms.push(format!("{word}es"));
        } else {
            forms.push(format!("{word}s"));
        }
    }

    forms.retain(|f| f != word);
    forms.sort();
    forms.dedup();
    forms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(word: &str, form: &str) -> bool {
        plural_forms(word).iter().any(|f| f == form)
    }

    #[test]
    fn singular_to_plural() {
        assert!(has("headphone", "headphones"));
        assert!(has("battery", "batteries"));
        assert!(has("box", "boxes"));
        assert!(has("watch", "watches"));
        assert!(has("glass", "glasses"));
        assert!(has("toy", "toys")); // vowel before y
        assert!(has("knife", "knives"));
    }

    #[test]
    fn plural_to_singular() {
        assert!(has("headphones", "headphone"));
        assert!(has("batteries", "battery"));
        assert!(has("boxes", "box"));
        assert!(has("shoes", "shoe"));
        assert!(has("glasses", "glass"));
        assert!(has("children", "child"));
        assert!(has("mice", "mouse"));
    }

    #[test]
    fn leaves_short_and_non_english_words_alone() {
        assert!(plural_forms("tv").is_empty());
        assert!(plural_forms("1996").is_empty());
        assert!(plural_forms("東京").is_empty());
        assert!(!has("glass", "glas"));
        assert!(!plural_forms("kettles").contains(&"kettles".to_owned()));
    }

    #[test]
    fn stop_words() {
        assert!(is_stop_word("the"));
        assert!(!is_stop_word("wild"));
    }
}
