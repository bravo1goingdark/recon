//! `recon-embed-client` — hosted embedding HTTP client.
//!
//! Implements [`recon_core::embed::EmbedService`] over the worker's
//! `POST /v1/embed` endpoint. The worker forwards (with KV caching)
//! to a Modal-hosted `jina-embeddings-v2-base-code` deployment; this
//! crate is the Rust client that talks to the worker.
//!
//! ## Why this exists
//!
//! See `docs/HOSTED_EMBED_PLAN.md` for the full rationale. Short
//! version: the local-inference path (`recon-embed`) pulls fastembed →
//! ort-sys (ONNX C++) → openssl-sys, inflating the binary from ~28 MB
//! to ~80 MB and breaking the `aarch64-unknown-linux-gnu`
//! cross-compile. Hosted inference keeps the binary lean while still
//! offering semantic search; source files never leave the user's
//! machine — only chunk text travels.
//!
//! ## Privacy
//!
//! - Source files stay local.
//! - Chunk text (tens to a few hundred tokens per chunk) is sent to
//!   the worker, which proxies to Modal. The worker caches by
//!   `sha256(text)` so repeated chunks across users hit Modal once.
//! - Set `RECON_NO_EMBED=1` to disable embedding entirely; semantic
//!   search degrades to lexical-only and nothing else is affected.
//! - Air-gapped users can rebuild `recon-cli` with
//!   `--features local-embed` for offline ONNX inference.
//!
//! ## API
//!
//! ```ignore
//! use recon_embed_client::HostedEmbedService;
//! use recon_core::embed::EmbedService;
//!
//! let svc = HostedEmbedService::from_env()
//!     .expect("login first with `recon login <key>`");
//! let vectors = svc.embed_batch(vec!["fn main() {}".into()])?;
//! assert_eq!(vectors[0].len(), 768);
//! # Ok::<_, recon_core::error::Error>(())
//! ```
//!
//! ## Internal env-var override
//!
//! `RECON_API_URL` overrides the hardcoded production worker URL.
//! This is a test/dev escape hatch — **not** part of the public API
//! and intentionally absent from `site/Docs.html`. We don't want
//! paying users routing their embed traffic to a third-party worker.

#![deny(missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use recon_core::embed::{EmbedService, VECTOR_DIM};
use recon_core::error::Error;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// Production worker base URL. Override with `RECON_API_URL` for
/// dev/test only (intentionally undocumented in user-facing docs).
const DEFAULT_API_URL: &str = "https://recon-api.kumarashutosh34169.workers.dev";

/// Maximum chunks per worker round-trip. Matches the worker's
/// `MAX_TEXTS_PER_BATCH` cap (`worker/src/routes/embed.ts`). Batches
/// larger than this are sliced into multiple requests.
const MAX_BATCH_SIZE: usize = 64;

/// Maximum chars per chunk. Matches the worker's input validation;
/// chunks longer than this would be rejected with 400 anyway.
const MAX_CHARS_PER_CHUNK: usize = 8192;

/// Per-call HTTP timeout. The worker's own Modal-forwarding timeout
/// is 10 s; add headroom for KV reads and TLS.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// HTTP client for `POST /v1/embed`.
///
/// Cheap to clone — internally holds an `Arc<ureq::Agent>` for
/// connection pooling. Construct once at server startup and share.
#[derive(Clone)]
pub struct HostedEmbedService {
    base_url: Arc<str>,
    api_key: Arc<str>,
    agent: ureq::Agent,
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    texts: &'a [String],
}

#[derive(Deserialize)]
struct EmbedResponse {
    vectors: Vec<Vec<f32>>,
}

#[derive(Deserialize)]
struct EmbedErrorBody {
    error: Option<String>,
}

impl HostedEmbedService {
    /// Construct directly. For tests; production builds use
    /// [`Self::from_env`].
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .build()
            .into();
        Self {
            base_url: base_url.into().into(),
            api_key: api_key.into().into(),
            agent,
        }
    }

    /// Construct from environment + the cached credentials file.
    ///
    /// Returns `None` when no credentials are available (user has not
    /// run `recon login`) or when `RECON_NO_EMBED=1` is set. Callers
    /// should fall back to lexical-only search.
    ///
    /// `RECON_API_URL` is honoured as an internal override — useful
    /// for dev/test and self-hosted-worker scenarios. It is not
    /// documented in user-facing docs.
    pub fn from_env() -> Option<Self> {
        if std::env::var("RECON_NO_EMBED")
            .ok()
            .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false)
        {
            return None;
        }
        let config_dir = global_config_dir();
        let api_key = read_credentials(&config_dir)?;
        let base_url =
            std::env::var("RECON_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string());
        Some(Self::new(base_url, api_key))
    }

    /// Embed a single batch of ≤ [`MAX_BATCH_SIZE`] texts in one HTTP
    /// round-trip.
    fn embed_one_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
        debug_assert!(texts.len() <= MAX_BATCH_SIZE);
        let url = format!("{}/v1/embed", self.base_url);
        let body = EmbedRequest { texts };

        let response = self
            .agent
            .post(&url)
            .header("authorization", format!("Bearer {}", self.api_key).as_str())
            .header("content-type", "application/json")
            .send_json(&body);

        let mut response = match response {
            Ok(r) => r,
            Err(e) => {
                warn!(%e, "hosted-embed: HTTP transport error");
                return Err(Error::EmbedUnavailable);
            }
        };

        let status = response.status().as_u16();
        if status == 503 {
            warn!("hosted-embed: worker reported 503 (Modal upstream unavailable)");
            return Err(Error::EmbedUnavailable);
        }
        if status == 401 || status == 402 {
            return Err(Error::Embed(format!(
                "embed auth/tier rejection ({status}) — run `recon login <key>` and ensure your account has embed access"
            )));
        }
        if status == 429 {
            return Err(Error::Embed(
                "embed rate-limited — request was rejected by the worker; back off and retry"
                    .into(),
            ));
        }
        if !(200..300).contains(&status) {
            let body_text = response.body_mut().read_to_string().unwrap_or_default();
            let parsed: Option<EmbedErrorBody> = serde_json::from_str(&body_text).ok();
            let detail = parsed
                .and_then(|e| e.error)
                .unwrap_or_else(|| body_text.clone());
            return Err(Error::Embed(format!("embed worker {status}: {detail}")));
        }

        let parsed: EmbedResponse = match response.body_mut().read_json() {
            Ok(p) => p,
            Err(e) => {
                return Err(Error::Embed(format!("embed response parse: {e}")));
            }
        };
        if parsed.vectors.len() != texts.len() {
            return Err(Error::Embed(format!(
                "embed vector count mismatch: requested {}, got {}",
                texts.len(),
                parsed.vectors.len()
            )));
        }
        for (i, v) in parsed.vectors.iter().enumerate() {
            if v.len() != VECTOR_DIM {
                return Err(Error::Embed(format!(
                    "embed dim mismatch at index {i}: expected {VECTOR_DIM}, got {}",
                    v.len()
                )));
            }
        }
        Ok(parsed.vectors)
    }
}

impl EmbedService for HostedEmbedService {
    fn embed_batch(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, Error> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // Reject anything obviously over the model's context window
        // before we burn a round-trip on it.
        for (i, t) in texts.iter().enumerate() {
            if t.len() > MAX_CHARS_PER_CHUNK {
                return Err(Error::Embed(format!(
                    "chunk #{i} exceeds {MAX_CHARS_PER_CHUNK}-char limit ({} chars)",
                    t.len()
                )));
            }
        }
        if texts.len() <= MAX_BATCH_SIZE {
            return self.embed_one_batch(&texts);
        }
        // Slice into MAX_BATCH_SIZE-sized chunks, issue sequentially.
        // Concurrent batches would help large catch-ups but every
        // additional in-flight request multiplies the worker's KV
        // round-trips and the Modal warm-pool pressure; keep
        // sequential until benchmarks justify otherwise.
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(MAX_BATCH_SIZE) {
            let part = self.embed_one_batch(chunk)?;
            out.extend(part);
        }
        debug!(
            requested = texts.len(),
            returned = out.len(),
            "hosted-embed: chunked batch complete"
        );
        Ok(out)
    }

    fn vector_dim(&self) -> usize {
        VECTOR_DIM
    }
}

/// Mirrors `recon_server::license::global_config_dir`. Inlined here
/// so the embed client doesn't depend on `recon-server` (which would
/// create a Cargo cycle: server depends on this crate for the hosted
/// embed path).
fn global_config_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("RECON_CONFIG_DIR") {
        return std::path::PathBuf::from(dir);
    }
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("recon")
}

#[derive(Deserialize)]
struct StoredCredentials {
    key: String,
}

/// Mirrors `recon_server::license::read_credentials`. Same file
/// format (`<config_dir>/credentials.json` with a `key` field), same
/// idempotent semantics — missing file or empty key returns `None`.
fn read_credentials(config_dir: &std::path::Path) -> Option<String> {
    let path = config_dir.join("credentials.json");
    let body = std::fs::read_to_string(&path).ok()?;
    let stored: StoredCredentials = serde_json::from_str(&body).ok()?;
    if stored.key.is_empty() {
        return None;
    }
    Some(stored.key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_batch_returns_empty_without_http() {
        // No agent calls — the early return short-circuits before any
        // HTTP would be issued. Construct with a bogus URL to prove
        // it.
        let svc = HostedEmbedService::new("http://invalid.example", "sk-test");
        let result = svc.embed_batch(Vec::new()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn oversized_chunk_is_rejected_locally() {
        let svc = HostedEmbedService::new("http://invalid.example", "sk-test");
        let big = "x".repeat(MAX_CHARS_PER_CHUNK + 1);
        let err = svc.embed_batch(vec![big]).unwrap_err();
        assert!(matches!(err, Error::Embed(_)), "got {err:?}");
    }

    #[test]
    fn from_env_respects_recon_no_embed() {
        // Env vars are process-global; serialize access so parallel tests
        // don't race on the same variable.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        unsafe { std::env::set_var("RECON_NO_EMBED", "1") };
        let svc = HostedEmbedService::from_env();
        unsafe { std::env::remove_var("RECON_NO_EMBED") };
        assert!(svc.is_none(), "RECON_NO_EMBED=1 must disable hosted embed");
    }

    #[test]
    fn vector_dim_matches_constant() {
        let svc = HostedEmbedService::new("http://invalid.example", "sk-test");
        assert_eq!(svc.vector_dim(), VECTOR_DIM);
    }
}
