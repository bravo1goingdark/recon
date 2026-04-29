//! Backend-agnostic embedding interface.
//!
//! Two implementations live in the workspace:
//!
//! - `recon-embed::EmbedService` — local fastembed/ONNX inference.
//!   Pulled in via `recon-server`'s `local-embed` feature flag for
//!   air-gapped users; default builds skip it to avoid the
//!   `openssl-sys` / ONNX runtime dependency chain.
//! - `recon-embed-client::HostedEmbedService` — POST `/v1/embed`
//!   to the recon worker, which forwards to a Modal-hosted
//!   `jina-embeddings-v2-base-code` deployment. The default in
//!   stock `recon-cli` builds.
//!
//! Both impls present the same surface so [`recon-server`] holds an
//! `Arc<dyn EmbedService>` and switches at construction time without
//! cfg-gating every call site.
//!
//! ## Concurrency
//!
//! `EmbedService` is `Send + Sync`. Implementations may serialise
//! requests internally (the local impl uses a single worker thread; the
//! hosted impl issues HTTP calls in parallel up to its own
//! parallelism cap) — callers do not need to wrap in a mutex.
//!
//! ## Vector dimension
//!
//! Both impls return 768-dim L2-normalised vectors today
//! (`jina-embeddings-v2-base-code`). The vector store schema is
//! pinned to this dimension; changing it requires a coordinated
//! rebuild. See `docs/HOSTED_EMBED_PLAN.md` "model swap policy".

use crate::error::Error;

/// Vector dimension produced by the hosted and local embedders.
///
/// Pinned because the SQLite vector store schema, the search
/// pipeline, and the watcher catch-up all assume this constant.
/// Bumping it is a coordinated migration, not a drop-in change.
pub const VECTOR_DIM: usize = 768;

/// Backend-agnostic embedding service.
///
/// Returns 768-dim L2-normalised vectors. Sync API — implementations
/// that block (HTTP, ONNX inference) should be invoked from
/// `tokio::task::spawn_blocking` if called on an async runtime.
pub trait EmbedService: Send + Sync {
    /// Embed a batch of texts in a single round-trip when the backend
    /// supports it. Implementations that have a smaller native batch
    /// limit (e.g. the hosted endpoint caps at 64 per request) may
    /// internally chunk and concatenate.
    ///
    /// Empty input must return `Ok(vec![])` without performing any
    /// network or CPU work.
    fn embed_batch(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, Error>;

    /// Embed a single text. Default implementation calls
    /// [`Self::embed_batch`] with a single-element vec; backends that
    /// have a more efficient single-text path may override.
    fn embed_one(&self, text: &str) -> Result<Vec<f32>, Error> {
        let mut vecs = self.embed_batch(vec![text.to_string()])?;
        vecs.pop()
            .ok_or_else(|| Error::Embed("empty embed result".into()))
    }

    /// Reported vector dimension. Defaults to [`VECTOR_DIM`]; backends
    /// that swap to a different model must override.
    fn vector_dim(&self) -> usize {
        VECTOR_DIM
    }
}
