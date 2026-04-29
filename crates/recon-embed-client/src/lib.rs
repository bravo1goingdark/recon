//! `recon-embed-client` — hosted embedding HTTP client.
//!
//! **This crate is a scaffold.** The implementation plan lives in
//! `docs/HOSTED_EMBED_PLAN.md` §3. The crate compiles (zero items
//! emitted) so the workspace dependency graph and CI gates work
//! against the planned shape; the public API + tests land alongside
//! the worker route.
//!
//! When this crate ships, replace this module-level doc with the
//! actual surface description, drop the `#![allow(dead_code)]`, and
//! delete the FUTURE.md hosted-embeddings section in the same commit.

#![deny(missing_docs)]
#![allow(dead_code)] // Scaffold — see HOSTED_EMBED_PLAN.md §3.

/// 768-dim vector returned by the model. Pinned because all callers
/// (vector-store schema, search pipeline, watcher catch-up) assume
/// this dimension — changing it requires a coordinated cache + index
/// rebuild documented in the plan's "model swap policy" row.
pub const VECTOR_DIM: usize = 768;
