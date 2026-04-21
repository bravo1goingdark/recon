//! Local ONNX embeddings and vector search (feature-gated).
#![deny(missing_docs)]
//!
//! Provides [`Embedder`] (fastembed + jina-v2-base-code), [`EmbedService`]
//! (lock-free channel wrapper around the embedder), [`VectorStore`]
//! (write-only sqlite-vec connection), and [`VecReadPool`] (lock-free read
//! pool) for optional semantic search.

pub mod embed_service;
pub mod embedder;
pub mod error;
pub mod vec_read_pool;
pub mod vector_store;

pub use embed_service::EmbedService;
pub use embedder::Embedder;
pub use error::EmbedError;
pub use vec_read_pool::VecReadPool;
pub use vector_store::{EmbedEntry, VectorStore};
