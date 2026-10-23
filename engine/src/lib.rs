//! A search engine modeled on Algolia's design: prefix-aware inverted index,
//! query-time typo tolerance and tie-breaking ranking.

pub mod store;
pub mod tokenizer;

pub use store::{DocId, DocumentStore, StoreError};
pub use tokenizer::{Token, normalize, tokenize, tokenize_value};
