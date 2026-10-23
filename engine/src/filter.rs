//! Algolia's `filters` syntax, parsed into a tree and evaluated to a set of
//! record ids.
//!
//! ```text
//! brand:Élan AND price < 100
//! (color:Black OR color:White) AND NOT in_stock:false
//! category:"Home > Kitchen" AND rating:4 TO 5
//! ```
//!
//! - `attr:value` matches a facet value (case- and accent-insensitive). For an
//!   attribute that is not a facet, a numeric value means numeric equality.
//! - `attr < n`, `<=`, `=`, `!=`, `>=`, `>` and `attr:a TO b` (inclusive)
//!   compare numbers.
//! - `AND` binds tighter than `OR`; `NOT` negates; parentheses group.

use crate::index::Index;
use roaring::RoaringBitmap;
use std::fmt;
use std::ops::Bound::{Excluded, Included, Unbounded};

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    Facet { attribute: String, value: String },
    Numeric { attribute: String, op: NumericOp, value: f64 },
    Range { attribute: String, low: f64, high: f64 },
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Box<Filter>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericOp {
    Lt,
    Le,
    Eq,
    Ne,
    Ge,
    Gt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterError(pub String);

impl fmt::Display for FilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid filter: {}", self.0)
    }
}

impl std::error::Error for FilterError {}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// A bare word: attribute name, value, number, or keyword.
    Word(String),
    /// A double-quoted value; never a keyword.
    Quoted(String),
    Colon,
    Op(NumericOp),
    Open,
    Close,
}

fn lex(input: &str) -> Result<Vec<Token>, FilterError> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '(' => {
                chars.next();
                tokens.push(Token::Open);
            }
            ')' => {
                chars.next();
                tokens.push(Token::Close);
            }
            ':' => {
                chars.next();
                tokens.push(Token::Colon);
            }
            '<' | '>' | '=' | '!' => {
                chars.next();
                let with_eq = chars.peek() == Some(&'=');
                if with_eq {
                    chars.next();
                }
                let op = match (c, with_eq) {
                    ('<', false) => NumericOp::Lt,
                    ('<', true) => NumericOp::Le,
                    ('>', false) => NumericOp::Gt,
                    ('>', true) => NumericOp::Ge,
                    ('=', _) => NumericOp::Eq,
                    ('!', true) => NumericOp::Ne,
                    _ => return Err(FilterError("'!' must be followed by '='".into())),
                };
                tokens.push(Token::Op(op));
            }
            '"' => {
                chars.next();
                let mut value = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(escaped) => value.push(escaped),
                            None => return Err(FilterError("unterminated quote".into())),
                        },
                        Some(c) => value.push(c),
                        None => return Err(FilterError("unterminated quote".into())),
                    }
                }
                tokens.push(Token::Quoted(value));
            }
            _ => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || "():<>=!\"".contains(c) {
                        break;
                    }
                    word.push(c);
                    chars.next();
                }
                tokens.push(Token::Word(word));
            }
        }
    }
    Ok(tokens)
}

pub fn parse(input: &str) -> Result<Filter, FilterError> {
    let tokens = lex(input)?;
    let mut parser = Parser { tokens, pos: 0 };
    let filter = parser.or()?;
    match parser.tokens.get(parser.pos) {
        None => Ok(filter),
        Some(t) => Err(FilterError(format!("unexpected {}", describe(t)))),
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

fn describe(token: &Token) -> String {
    match token {
        Token::Word(w) => format!("'{w}'"),
        Token::Quoted(q) => format!("\"{q}\""),
        Token::Colon => "':'".into(),
        Token::Op(_) => "comparison operator".into(),
        Token::Open => "'('".into(),
        Token::Close => "')'".into(),
    }
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        token
    }

    fn keyword(&mut self, keyword: &str) -> bool {
        if matches!(self.peek(), Some(Token::Word(w)) if w == keyword) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn or(&mut self) -> Result<Filter, FilterError> {
        let mut parts = vec![self.and()?];
        while self.keyword("OR") {
            parts.push(self.and()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { Filter::Or(parts) })
    }

    fn and(&mut self) -> Result<Filter, FilterError> {
        let mut parts = vec![self.unary()?];
        while self.keyword("AND") {
            parts.push(self.unary()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { Filter::And(parts) })
    }

    fn unary(&mut self) -> Result<Filter, FilterError> {
        if self.keyword("NOT") {
            return Ok(Filter::Not(Box::new(self.unary()?)));
        }
        if self.peek() == Some(&Token::Open) {
            self.pos += 1;
            let inner = self.or()?;
            return match self.next() {
                Some(Token::Close) => Ok(inner),
                _ => Err(FilterError("missing ')'".into())),
            };
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Filter, FilterError> {
        let attribute = match self.next() {
            Some(Token::Word(w)) if !matches!(w.as_str(), "AND" | "OR" | "NOT" | "TO") => w,
            Some(Token::Quoted(q)) => q,
            Some(t) => return Err(FilterError(format!("expected an attribute, found {}", describe(&t)))),
            None => return Err(FilterError("expected an attribute, found the end".into())),
        };
        match self.next() {
            Some(Token::Colon) => {
                let value = self.value(&attribute)?;
                if self.keyword("TO") {
                    let high_text = self.value(&attribute)?;
                    let low = number(&attribute, &value)?;
                    let high = number(&attribute, &high_text)?;
                    Ok(Filter::Range { attribute, low, high })
                } else {
                    Ok(Filter::Facet { attribute, value })
                }
            }
            Some(Token::Op(op)) => {
                let text = self.value(&attribute)?;
                let value = number(&attribute, &text)?;
                Ok(Filter::Numeric { attribute, op, value })
            }
            _ => Err(FilterError(format!("expected ':' or a comparison after '{attribute}'"))),
        }
    }

    fn value(&mut self, attribute: &str) -> Result<String, FilterError> {
        match self.next() {
            Some(Token::Word(w)) | Some(Token::Quoted(w)) => Ok(w),
            _ => Err(FilterError(format!("missing value for '{attribute}'"))),
        }
    }
}

fn number(attribute: &str, text: &str) -> Result<f64, FilterError> {
    text.parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .ok_or_else(|| FilterError(format!("'{attribute}' needs a number, got '{text}'")))
}

impl Filter {
    /// The set of records matching this filter.
    pub fn evaluate(&self, index: &Index) -> Result<RoaringBitmap, FilterError> {
        Ok(match self {
            Filter::Facet { attribute, value } => {
                if index.is_facet(attribute) {
                    index.facet_docs(attribute, value)
                } else if let Ok(n) = value.parse::<f64>() {
                    index.numeric_docs(attribute, Included(n), Included(n))
                } else {
                    return Err(FilterError(format!(
                        "'{attribute}' is not in attributesForFaceting"
                    )));
                }
            }
            Filter::Numeric { attribute, op, value } => {
                let v = *value;
                let (low, high) = match op {
                    NumericOp::Lt => (Unbounded, Excluded(v)),
                    NumericOp::Le => (Unbounded, Included(v)),
                    NumericOp::Eq => (Included(v), Included(v)),
                    NumericOp::Ne => {
                        return Ok(index.all_docs() - index.numeric_docs(attribute, Included(v), Included(v)));
                    }
                    NumericOp::Ge => (Included(v), Unbounded),
                    NumericOp::Gt => (Excluded(v), Unbounded),
                };
                index.numeric_docs(attribute, low, high)
            }
            Filter::Range { attribute, low, high } => {
                index.numeric_docs(attribute, Included(*low), Included(*high))
            }
            Filter::And(parts) => {
                let mut result = parts[0].evaluate(index)?;
                for part in &parts[1..] {
                    result &= part.evaluate(index)?;
                }
                result
            }
            Filter::Or(parts) => {
                let mut result = RoaringBitmap::new();
                for part in parts {
                    result |= part.evaluate(index)?;
                }
                result
            }
            Filter::Not(inner) => index.all_docs() - inner.evaluate(index)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facet(attribute: &str, value: &str) -> Filter {
        Filter::Facet { attribute: attribute.into(), value: value.into() }
    }

    #[test]
    fn parses_facets_numbers_and_ranges() {
        assert_eq!(parse("brand:Élan").unwrap(), facet("brand", "Élan"));
        assert_eq!(
            parse("price<=99.5").unwrap(),
            Filter::Numeric { attribute: "price".into(), op: NumericOp::Le, value: 99.5 }
        );
        assert_eq!(
            parse("rating:4 TO 5").unwrap(),
            Filter::Range { attribute: "rating".into(), low: 4.0, high: 5.0 }
        );
        assert_eq!(parse(r#"category:"Home > Kitchen""#).unwrap(), facet("category", "Home > Kitchen"));
        assert_eq!(
            parse("year != -3").unwrap(),
            Filter::Numeric { attribute: "year".into(), op: NumericOp::Ne, value: -3.0 }
        );
    }

    #[test]
    fn and_binds_tighter_than_or_and_parentheses_group() {
        assert_eq!(
            parse("a:1 OR b:2 AND c:3").unwrap(),
            Filter::Or(vec![facet("a", "1"), Filter::And(vec![facet("b", "2"), facet("c", "3")])])
        );
        assert_eq!(
            parse("(a:1 OR b:2) AND NOT c:3").unwrap(),
            Filter::And(vec![
                Filter::Or(vec![facet("a", "1"), facet("b", "2")]),
                Filter::Not(Box::new(facet("c", "3"))),
            ])
        );
    }

    #[test]
    fn reports_readable_errors() {
        let err = |s: &str| parse(s).unwrap_err().0;
        assert_eq!(err("brand"), "expected ':' or a comparison after 'brand'");
        assert_eq!(err("price < cheap"), "'price' needs a number, got 'cheap'");
        assert_eq!(err("(a:1"), "missing ')'");
        assert_eq!(err("a:1 b:2"), "unexpected 'b'");
        assert_eq!(err(r#"a:"open"#), "unterminated quote");
        assert_eq!(err("a:1 AND"), "expected an attribute, found the end");
        assert_eq!(err("a !1"), "'!' must be followed by '='");
    }
}
