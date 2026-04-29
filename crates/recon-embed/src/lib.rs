//! Local ONNX embeddings and vector search (feature-gated).
#![deny(missing_docs)]
//!
//! Two layers, separately gated:
//!
//! - **Always linked**: [`VectorStore`] (write-only sqlite-vec
//!   connection) and [`VecReadPool`] (lock-free read pool). These
//!   pull only `rusqlite` + `sqlite-vec` and stay available in
//!   default `recon-cli` builds.
//! - **`local-inference` feature**: [`Embedder`] (fastembed +
//!   jina-v2-base-code) and [`EmbedService`] (lock-free channel
//!   wrapper). Pulled in for air-gapped users; default builds use
//!   the hosted client (`recon-embed-client`) instead.

pub mod error;
pub mod format;
pub mod vec_read_pool;
pub mod vector_store;

pub use format::format_symbol;

pub use error::EmbedError;
pub use vec_read_pool::VecReadPool;
pub use vector_store::{EmbedEntry, VectorStore};

#[cfg(feature = "local-inference")]
pub mod embed_service;
#[cfg(feature = "local-inference")]
pub mod embedder;

#[cfg(feature = "local-inference")]
pub use embed_service::EmbedService;
#[cfg(feature = "local-inference")]
pub use embedder::Embedder;
