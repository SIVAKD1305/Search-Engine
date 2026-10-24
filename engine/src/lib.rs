//! A search engine modeled on Algolia's design: prefix-aware inverted index,
//! query-time typo tolerance and tie-breaking ranking.

pub mod bucket;
pub mod facets;
pub mod filter;
pub mod geo;
pub mod highlight;
pub mod index;
pub mod language;
pub mod ranking;
pub mod search;
pub mod store;
pub mod synonyms;
pub mod tokenizer;
pub mod typo;

pub use index::{Index, Settings};
pub use store::{DocId, DocumentStore, StoreError};
pub use tokenizer::{Token, normalize, tokenize, tokenize_value};
