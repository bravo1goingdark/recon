//! Local ONNX embeddings and vector search (feature-gated).
#![deny(missing_docs)]
//!
//! Provides [`Embedder`] (fastembed + jina-v2-base-code) and [`VectorStore`] (LanceDB)
//! for optional semantic search fallback.

pub mod embedder;
pub mod error;
pub mod vector_store;

pub use embedder::Embedder;
pub use error::EmbedError;
pub use vector_store::{EmbedEntry, VectorStore};
