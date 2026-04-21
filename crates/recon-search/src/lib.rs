//! Text search, Tantivy structured search, fuzzy matching, and hybrid search.
#![deny(missing_docs)]

pub mod fff_backend;
pub mod filters;
pub mod fuzzy;
pub mod pagerank;
pub mod search_trait;
pub mod tantivy_backend;
pub mod text;
pub mod tokens;
pub(crate) mod utils;
