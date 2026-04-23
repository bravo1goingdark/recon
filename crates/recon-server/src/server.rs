//! MCP server — fully wired: Tantivy search, PageRank, redaction, live watching.

use crate::tools::*;
use ahash::AHashMap;
use arc_swap::ArcSwap;
use compact_str::CompactString;
use parking_lot::Mutex;
use rayon::prelude::*;
use recon_core::error::ReconErrorCode;
use recon_core::lang::Language;
use recon_core::redact;
use recon_core::shapes::*;
use recon_indexer::indexer;
use recon_indexer::walker;
use recon_indexer::watcher::Watcher;
use recon_parser::pool::LanguagePools;
use recon_search::fff_backend::FffBackend;
use recon_search::search_trait::{TextQuery, TextSearcher};
use recon_search::{filters, fuzzy, pagerank, tantivy_backend::TantivyBackend};
use recon_storage::read_pool::ReadPool;
use recon_storage::store::Store;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use smallvec::SmallVec;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn, Instrument};

/// The recon MCP server.
#[derive(Clone)]
pub struct ReconServer {
    #[allow(dead_code)] // read by the #[tool_router] macro expansion
    tool_router: ToolRouter<Self>,
    /// Write-only store. Only used by watcher, reindex, and cache updates.
    write_store: Arc<Mutex<Store>>,
    /// Lock-free read pool — concurrent tool queries go through here.
    read_pool: Arc<ReadPool>,
    tantivy: Arc<TantivyBackend>,
    /// Single shared Tantivy IndexWriter — Tantivy enforces exactly one writer
    /// per directory. Shared between initial indexing, watcher, and reindex tool.
    tantivy_writer: Arc<Mutex<Option<tantivy::IndexWriter>>>,
    text_searcher: Arc<dyn TextSearcher>,
    repo_root: PathBuf,
    /// Cached file paths — invalidated on index/reindex. Avoids SQLite query on every tool call.
    cached_paths: Arc<ArcSwap<Vec<PathBuf>>>,
    /// Cached file count — updated on index/reindex. Avoids loading all paths just for count.
    cached_file_count: Arc<AtomicU64>,
    /// Cached all_symbols — avoids 80MB+ alloc on every code_repo_map call.
    cached_symbols: Arc<ArcSwap<Vec<recon_core::symbol::Symbol>>>,
    /// Cached all_refs — avoids alloc on every code_repo_map call.
    cached_refs: Arc<ArcSwap<Vec<recon_core::symbol::Ref>>>,
    /// Lock-free embedding service — shared via Arc, no mutex on hot path.
    #[cfg(feature = "embed")]
    embed_service: Arc<Mutex<Option<Arc<recon_embed::EmbedService>>>>,
    /// Lock-free read pool for vector similarity search.
    #[cfg(feature = "embed")]
    vec_read_pool: Arc<Mutex<Option<Arc<recon_embed::VecReadPool>>>>,
    /// Write handle — taken by `start_watcher`, None afterwards.
    #[cfg(feature = "embed")]
    vec_writer: Arc<Mutex<Option<recon_embed::VectorStore>>>,
    /// Cooperative shutdown flag — watcher loop polls this between batches.
    shutdown_flag: Arc<AtomicBool>,
    /// Handle to the spawned watcher task so `shutdown()` can await its exit.
    watcher_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

fn redact_response(response: String) -> String {
    redact::redact_secrets(&response).unwrap_or(response)
}

/// Maximum file size for `read_to_string` calls (2 MB).
/// Prevents OOM on accidentally large files (e.g. minified bundles, lock files).
const MAX_READ_FILE_SIZE: u64 = 2 * 1024 * 1024;

/// Per-request deadline. Queries longer than this return `ToolOutput::Error`
/// with `ReconErrorCode::Timeout` rather than hanging the client.
/// Override with `RECON_REQUEST_TIMEOUT_SECS`.
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

tokio::task_local! {
    /// Per-tool-call request ID (ULID). Set by [`ReconServer::query_tool`] so
    /// error responses and log spans carry the same correlation handle.
    pub static REQUEST_ID: CompactString;
}

/// Return the active request ID, or `"-"` if we're called outside a scoped
/// request (e.g. direct rmcp dispatch). Never panics.
pub(crate) fn current_request_id() -> CompactString {
    REQUEST_ID
        .try_with(|id| id.clone())
        .unwrap_or_else(|_| CompactString::new("-"))
}

/// Read the per-request timeout from `RECON_REQUEST_TIMEOUT_SECS` once per
/// call. Bounded to `[1, 600]` seconds so a typo in env never wedges the
/// server or disables the guard.
fn request_timeout() -> std::time::Duration {
    let secs = std::env::var("RECON_REQUEST_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS)
        .clamp(1, 600);
    std::time::Duration::from_secs(secs)
}

/// Build a structured `ToolOutput::Error` and serialize it for the wire.
///
/// Uses the currently-scoped request ID (falls back to `"-"` for direct
/// rmcp dispatch). `data` is free-form JSON the client can inspect for
/// the failing path, size, identifier, etc.
pub(crate) fn tool_error(
    code: ReconErrorCode,
    message: impl Into<String>,
    data: Option<serde_json::Value>,
) -> String {
    let view = ToolOutput::Error(ToolErrorView {
        code: code.code(),
        kind: CompactString::new(code.kind()),
        message: message.into(),
        data,
        request_id: current_request_id(),
    });
    serde_json::to_string(&view).unwrap_or_else(|e| {
        // Absolute fallback — serde_json on a pure-Serialize value almost never
        // fails, but if it does we must still produce SOME parseable response.
        format!(
            r#"{{"shape":"Error","code":{},"kind":"{}","message":"serialize failed: {}","request_id":"-"}}"#,
            code.code(),
            code.kind(),
            e
        )
    })
}

/// Convenience for the common case: map a `recon_core::error::Error` through
/// its `rpc_code` and render. The error's `Display` is used for the message.
pub(crate) fn tool_error_from(err: &recon_core::error::Error) -> String {
    tool_error(err.rpc_code(), err.to_string(), None)
}

/// Tool-error specifically for invalid JSON args.
pub(crate) fn tool_error_invalid_args(err: &serde_json::Error) -> String {
    tool_error(
        ReconErrorCode::InvalidParams,
        format!("invalid tool arguments: {err}"),
        None,
    )
}

impl ReconServer {
    /// Create a new server for the given repo root.
    ///
    /// Creates a single Tantivy `IndexWriter` that is shared between initial
    /// indexing, the file watcher, and the `code_reindex` tool. This prevents
    /// the `LockBusy` error from competing writers.
    ///
    /// # Errors
    /// Returns `Err` if the in-memory read pool cannot be created (should not
    /// happen in practice for in-memory stores, but propagated rather than panicking).
    pub fn new(
        repo_root: PathBuf,
        store: Store,
        tantivy: TantivyBackend,
    ) -> Result<Self, recon_core::error::Error> {
        let writer = tantivy.writer(50_000_000).ok();
        if writer.is_none() {
            warn!("tantivy writer creation failed at startup");
        }
        // Create a lock-free read pool from the same DB file (4 connections).
        // Falls back to an in-memory pool for in-memory stores (tests).
        let read_pool = match store.db_path().and_then(|p| ReadPool::new(p, 4).ok()) {
            Some(pool) => Arc::new(pool),
            None => {
                warn!("no on-disk DB path; creating in-memory read pool (tests only)");
                Arc::new(
                    ReadPool::new(std::path::Path::new(":memory:"), 1)
                        .map_err(|e| recon_core::error::Error::Storage(e.to_string()))?,
                )
            }
        };
        Ok(Self {
            tool_router: Self::tool_router(),
            write_store: Arc::new(Mutex::new(store)),
            read_pool,
            tantivy: Arc::new(tantivy),
            tantivy_writer: Arc::new(Mutex::new(writer)),
            text_searcher: Arc::new(FffBackend::new()),
            repo_root,
            cached_paths: Arc::new(ArcSwap::new(Arc::new(Vec::new()))),
            cached_file_count: Arc::new(AtomicU64::new(0)),
            cached_symbols: Arc::new(ArcSwap::new(Arc::new(Vec::new()))),
            cached_refs: Arc::new(ArcSwap::new(Arc::new(Vec::new()))),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            watcher_handle: Arc::new(Mutex::new(None)),
            #[cfg(feature = "embed")]
            embed_service: Arc::new(Mutex::new(None)),
            #[cfg(feature = "embed")]
            vec_read_pool: Arc::new(Mutex::new(None)),
            #[cfg(feature = "embed")]
            vec_writer: Arc::new(Mutex::new(None)),
        })
    }

    /// Refresh all cached data from the database.
    /// Called after initial index and reindex to keep caches warm.
    fn refresh_caches(&self) {
        match self.read_pool.all_file_paths() {
            Ok(paths) => {
                self.cached_file_count
                    .store(paths.len() as u64, Ordering::Relaxed);
                self.cached_paths.store(Arc::new(paths));
            }
            Err(e) => warn!("failed to refresh path cache: {e}"),
        }
        match self.read_pool.all_symbols() {
            Ok(syms) => self.cached_symbols.store(Arc::new(syms)),
            Err(e) => warn!("failed to refresh symbols cache: {e}"),
        }
        match self.read_pool.all_refs() {
            Ok(refs) => self.cached_refs.store(Arc::new(refs)),
            Err(e) => warn!("failed to refresh refs cache: {e}"),
        }
    }

    /// Get cached file paths — avoids SQLite query on hot path.
    fn cached_file_paths(&self) -> Arc<Vec<PathBuf>> {
        let guard = self.cached_paths.load();
        if guard.is_empty() {
            // Cache cold — populate on first access.
            match self.read_pool.all_file_paths() {
                Ok(loaded) => {
                    self.cached_file_count
                        .store(loaded.len() as u64, Ordering::Relaxed);
                    let arc = Arc::new(loaded);
                    self.cached_paths.store(arc.clone());
                    arc
                }
                Err(_) => guard.clone(),
            }
        } else {
            guard.clone()
        }
    }

    /// Get cached all_symbols — avoids 80MB+ alloc on hot path.
    fn cached_all_symbols(&self) -> Arc<Vec<recon_core::symbol::Symbol>> {
        let guard = self.cached_symbols.load();
        if guard.is_empty() {
            match self.read_pool.all_symbols() {
                Ok(loaded) => {
                    let arc = Arc::new(loaded);
                    self.cached_symbols.store(arc.clone());
                    arc
                }
                Err(_) => guard.clone(),
            }
        } else {
            guard.clone()
        }
    }

    /// Get cached all_refs — avoids alloc on hot path.
    fn cached_all_refs(&self) -> Arc<Vec<recon_core::symbol::Ref>> {
        let guard = self.cached_refs.load();
        if guard.is_empty() {
            match self.read_pool.all_refs() {
                Ok(loaded) => {
                    let arc = Arc::new(loaded);
                    self.cached_refs.store(arc.clone());
                    arc
                }
                Err(_) => guard.clone(),
            }
        } else {
            guard.clone()
        }
    }

    /// Initialize the embedding engine (model download on first run).
    ///
    /// Spawns the embed worker thread and opens the vector store. After this
    /// returns, [`start_watcher`] must be called to hand the write handle to
    /// the watcher — `vec_writer` is intentionally `None` until then.
    ///
    /// [`start_watcher`]: ReconServer::start_watcher
    #[cfg(feature = "embed")]
    pub async fn init_embed(&self) -> Result<(), recon_core::error::Error> {
        let vec_dir = self.repo_root.join(".recon").join("vectors");

        let embedder = if let Ok(dir) = std::env::var("RECON_EMBED_DIR") {
            let model_dir = std::path::Path::new(&dir);
            info!(dir = %dir, "using local embedding model");
            recon_embed::Embedder::from_local_model(model_dir)
                .map_err(|e| recon_core::error::Error::Search(format!("local embed init: {e}")))?
        } else {
            recon_embed::Embedder::new()
                .map_err(|e| recon_core::error::Error::Search(format!("embed init: {e}")))?
        };
        let svc =
            Arc::new(recon_embed::EmbedService::spawn(embedder).map_err(|e| {
                recon_core::error::Error::Search(format!("embed thread spawn: {e}"))
            })?);
        let vs = recon_embed::VectorStore::open(&vec_dir)
            .map_err(|e| recon_core::error::Error::Search(format!("vector store open: {e}")))?;
        let pool = Arc::new(
            recon_embed::VecReadPool::new(&vec_dir, 4)
                .map_err(|e| recon_core::error::Error::Search(format!("vec read pool: {e}")))?,
        );
        *self.embed_service.lock() = Some(svc);
        *self.vec_read_pool.lock() = Some(pool);
        *self.vec_writer.lock() = Some(vs);
        info!("embedding engine initialized");
        Ok(())
    }

    /// Run initial indexing of the repo (SQLite + Tantivy).
    pub async fn index_repo(&self) -> Result<(), recon_core::error::Error> {
        let store = self.write_store.lock();
        let mut tw = self.tantivy_writer.lock();
        let stats = indexer::index_repo_incremental(
            &store,
            Some(&self.tantivy),
            &self.repo_root,
            tw.as_mut(),
        )?;
        // VACUUM after bulk indexing to reclaim free pages and shrink the DB file.
        store
            .incremental_vacuum()
            .map_err(|e| recon_core::error::Error::Storage(format!("incremental_vacuum: {e}")))?;
        info!(
            files = stats.files_indexed,
            symbols = stats.total_symbols,
            "initial indexing complete"
        );
        drop(tw);

        // Pre-warm the file path cache so tool calls don't hit SQLite on first query.
        drop(store);
        self.refresh_caches();
        let store = self.write_store.lock();

        // Pre-warm the repo_map cache so the first user call is instant.
        // Uses cached symbols/refs from refresh_caches() above.
        if stats.total_symbols > 0 {
            let all_symbols = self.cached_all_symbols();
            let all_refs = self.cached_all_refs();
            let last_idx = self.read_pool.max_indexed_at().unwrap_or(0);
            let budget = 2000;
            let cache_key = format!("map_cache:{last_idx}:{budget}");

            let ranked = pagerank::pagerank(
                &all_symbols,
                &all_refs,
                &[],
                0.85,
                pagerank::DEFAULT_MAX_ITERATIONS,
            );
            let content = pagerank::render_repo_map(&all_symbols, &ranked, budget);
            let token_est = recon_search::tokens::estimate_tokens(&content);
            let view = ToolOutput::Skeleton(SkeletonView {
                path: None,
                content,
                token_estimate: token_est,
            });
            let result = redact_response(
                serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")),
            );

            let _ = store.delete_meta_prefix("map_cache:");
            let _ = store.set_meta(&cache_key, &result);
            info!("repo_map cache pre-warmed");
        }

        Ok(())
    }

    /// Dispatch a tool call by name with JSON arguments. For CLI `query` subcommand.
    ///
    /// Returns the tool's JSON response string, or an error message.
    pub async fn query_tool(&self, tool_name: &str, args_json: &str) -> String {
        // Generate a ULID per call and scope it into task-local state so any
        // `tool_error*` helper invoked downstream carries the same handle.
        // The `tracing` span attaches the same ID so a client-side request_id
        // can be grepped back to server logs.
        let request_id = CompactString::new(ulid::Ulid::new().to_string());
        let tool_name_owned = tool_name.to_string();
        let args_owned = args_json.to_string();
        let timeout = request_timeout();
        let span = tracing::info_span!(
            "query_tool",
            tool = tool_name,
            request_id = %request_id,
        );

        REQUEST_ID
            .scope(request_id.clone(), async move {
                let fut = self.dispatch_tool(&tool_name_owned, &args_owned);
                match tokio::time::timeout(timeout, fut).await {
                    Ok(response) => response,
                    Err(_) => {
                        tracing::warn!(
                            tool = %tool_name_owned,
                            %request_id,
                            timeout_secs = timeout.as_secs(),
                            "tool call exceeded deadline",
                        );
                        tool_error(
                            ReconErrorCode::Timeout,
                            format!(
                                "tool {tool_name_owned} exceeded {}s deadline",
                                timeout.as_secs()
                            ),
                            Some(serde_json::json!({
                                "tool": tool_name_owned,
                                "timeout_secs": timeout.as_secs(),
                            })),
                        )
                    }
                }
            })
            .instrument(span)
            .await
    }

    /// Inner dispatch. Separated from [`query_tool`] so the latter can wrap
    /// it in `task_local::scope` + `tokio::time::timeout`.
    async fn dispatch_tool(&self, tool_name: &str, args_json: &str) -> String {
        match tool_name {
            "code_outline" => match serde_json::from_str::<OutlineParams>(args_json) {
                Ok(p) => self.code_outline(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_skeleton" => match serde_json::from_str::<SkeletonParams>(args_json) {
                Ok(p) => self.code_skeleton(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_read_symbol" => match serde_json::from_str::<ReadSymbolParams>(args_json) {
                Ok(p) => self.code_read_symbol(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_find_symbol" => match serde_json::from_str::<FindSymbolParams>(args_json) {
                Ok(p) => self.code_find_symbol(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_find_refs" => match serde_json::from_str::<FindRefsParams>(args_json) {
                Ok(p) => self.code_find_refs(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_search" => match serde_json::from_str::<SearchParams>(args_json) {
                Ok(p) => self.code_search(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_list" => match serde_json::from_str::<ListParams>(args_json) {
                Ok(p) => self.code_list(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_repo_map" => match serde_json::from_str::<RepoMapParams>(args_json) {
                Ok(p) => self.code_repo_map(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_find_strings" => match serde_json::from_str::<FindStringsParams>(args_json) {
                Ok(p) => self.code_find_strings(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_multi_find" => match serde_json::from_str::<MultiFindParams>(args_json) {
                Ok(p) => self.code_multi_find(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_reindex" => match serde_json::from_str::<ReindexParams>(args_json) {
                Ok(p) => self.code_reindex(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_stats" => match serde_json::from_str::<StatsParams>(args_json) {
                Ok(p) => self.code_stats(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            other => tool_error(
                ReconErrorCode::NotFound,
                format!("unknown tool: {other}"),
                Some(serde_json::json!({ "tool": other })),
            ),
        }
    }

    /// Start background file watcher that re-indexes on changes.
    ///
    /// Parse-then-write architecture: parsing happens with NO locks held,
    /// then two short lock acquisitions (store, then tantivy) for the writes.
    /// This prevents the watcher from starving concurrent tool reads.
    pub fn start_watcher(&self) {
        let write_store = self.write_store.clone();
        let read_pool = self.read_pool.clone();
        let tantivy = self.tantivy.clone();
        let tantivy_writer = self.tantivy_writer.clone();
        let repo_root = self.repo_root.clone();
        let cached_paths = self.cached_paths.clone();
        let cached_file_count = self.cached_file_count.clone();
        let cached_symbols = self.cached_symbols.clone();
        let cached_refs = self.cached_refs.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        // Clone the Arc handles once; the hot path inside the loop needs no locks.
        #[cfg(feature = "embed")]
        let embed_svc: Option<Arc<recon_embed::EmbedService>> = self.embed_service.lock().clone();
        #[cfg(feature = "embed")]
        let vec_pool: Option<Arc<recon_embed::VecReadPool>> = self.vec_read_pool.lock().clone();
        // Take the write handle — watcher owns it exclusively from here.
        #[cfg(feature = "embed")]
        let vec_writer: Option<recon_embed::VectorStore> = self.vec_writer.lock().take();

        let handle = tokio::task::spawn_blocking(move || {
            let watcher = match Watcher::new(&repo_root) {
                Ok(w) => w,
                Err(e) => {
                    warn!("failed to start watcher: {e}");
                    return;
                }
            };
            info!("file watcher started");

            let pools = LanguagePools::new(1);

            // ── One-time catch-up: embed any symbols not yet in the vector store ──
            // Runs before the event loop so the watcher thread owns vec_writer exclusively.
            #[cfg(feature = "embed")]
            if let (Some(ref svc), Some(ref pool), Some(ref writer)) =
                (&embed_svc, &vec_pool, &vec_writer)
            {
                const EMBED_BATCH: usize = 64;
                match read_pool.all_symbols() {
                    Err(e) => warn!("embed catch-up: all_symbols: {e}"),
                    Ok(all_syms) if all_syms.is_empty() => {}
                    Ok(all_syms) => {
                        let all_ids: Vec<u64> = all_syms.iter().map(|s| s.id).collect();
                        let existing = pool.existing_hashes(&all_ids).unwrap_or_else(|e| {
                            warn!("embed catch-up: existing_hashes: {e}");
                            AHashMap::new()
                        });
                        let to_embed: Vec<&recon_core::symbol::Symbol> = all_syms
                            .iter()
                            .filter(|s| existing.get(&s.id).map_or(true, |h| *h != s.body_hash))
                            .collect();
                        if to_embed.is_empty() {
                            info!(
                                total = all_syms.len(),
                                "embed catch-up: all symbols already embedded"
                            );
                        } else {
                            info!(
                                total = all_syms.len(),
                                missing = to_embed.len(),
                                "embed catch-up: starting"
                            );
                            // Group by file so each source file is read exactly once.
                            let mut by_file: AHashMap<
                                &std::path::Path,
                                Vec<&recon_core::symbol::Symbol>,
                            > = AHashMap::new();
                            for s in &to_embed {
                                by_file.entry(s.path.as_path()).or_default().push(s);
                            }
                            let mut done = 0usize;
                            for (rel_path, syms) in &by_file {
                                let file_bytes = match std::fs::read(repo_root.join(rel_path)) {
                                    Ok(b) => b,
                                    Err(e) => {
                                        warn!(?rel_path, "embed catch-up: cannot read file: {e}");
                                        continue;
                                    }
                                };
                                for chunk in syms.chunks(EMBED_BATCH) {
                                    let texts: Vec<String> = chunk
                                        .iter()
                                        .map(|s| {
                                            let body = file_bytes
                                                .get(s.byte_range.clone())
                                                .and_then(|b| std::str::from_utf8(b).ok())
                                                .unwrap_or("");
                                            recon_embed::Embedder::format_symbol(s, body)
                                        })
                                        .collect();
                                    let vecs = match svc.embed_batch(texts) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            warn!("embed catch-up: embed_batch: {e}");
                                            continue;
                                        }
                                    };
                                    let entries: Vec<recon_embed::EmbedEntry> = chunk
                                        .iter()
                                        .zip(vecs)
                                        .map(|(s, vec)| recon_embed::EmbedEntry {
                                            id: s.id,
                                            qualified_name: s.qualified_name.to_string(),
                                            vector: vec,
                                            body_hash: s.body_hash.to_vec(),
                                            lang: s.lang.name().to_string(),
                                        })
                                        .collect();
                                    if let Err(e) = writer.upsert_embeddings(&entries) {
                                        warn!("embed catch-up: upsert: {e}");
                                    }
                                    done += chunk.len();
                                }
                            }
                            info!(done, "embed catch-up: complete");
                        }
                    }
                }
            }

            loop {
                if shutdown_flag.load(Ordering::Relaxed) {
                    debug!("watcher: shutdown flag set, exiting loop");
                    break;
                }
                let changed_paths = match watcher.recv_timeout(Duration::from_millis(500)) {
                    Ok(paths) => paths,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        debug!("watcher: channel disconnected, exiting loop");
                        break;
                    }
                };
                // catch_unwind: a panic in one batch must not kill the watcher
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // Phase 1: Filter to files that actually changed (lock-free via ReadPool)
                    let to_parse: Vec<(PathBuf, Vec<u8>)> = changed_paths
                        .into_iter()
                        .filter_map(|path| {
                            let content = std::fs::read(&path).ok()?;
                            if walker::is_generated_content(&content) {
                                return None;
                            }
                            let rel_path = path.strip_prefix(&repo_root).unwrap_or(&path);
                            let content_hash = recon_storage::hash::blake3_bytes(&content);
                            let unchanged = match read_pool.get_file_hash(rel_path) {
                                Ok(Some(h)) => h == content_hash,
                                Ok(None) => false,
                                Err(e) => {
                                    warn!(?rel_path, "hash check failed, will re-index: {e}");
                                    false
                                }
                            };
                            if unchanged {
                                return None;
                            }
                            Some((path, content))
                        })
                        .collect();

                    if to_parse.is_empty() {
                        return;
                    }

                    // Phase 2: Parse all files (NO locks held — pure CPU work)
                    let parsed: Vec<indexer::ParsedFile> = to_parse
                        .iter()
                        .filter_map(|(path, content)| {
                            let content_hash = recon_storage::hash::blake3_bytes(content);
                            let mtime = indexer::mtime_of(path);
                            indexer::parse_file_with_content(
                                content,
                                path,
                                &repo_root,
                                &pools,
                                content_hash,
                                mtime,
                            )
                        })
                        .collect();

                    if parsed.is_empty() {
                        return;
                    }

                    // Phase 3: Batch write to SQLite (short lock)
                    {
                        let store = write_store.lock();
                        let bulk: Vec<_> = parsed
                            .iter()
                            .map(|p| (&p.meta, p.symbols.as_slice(), p.refs.as_slice()))
                            .collect();
                        if let Err(e) = store.batch_index_files(&bulk) {
                            warn!("watcher batch store error: {e}");
                        }
                    }

                    // Phase 4: Batch write to Tantivy (short lock, separate from store)
                    {
                        let mut tw = tantivy_writer.lock();
                        if let Some(ref mut writer) = *tw {
                            for pf in &parsed {
                                let _ = tantivy.index_symbols(writer, &pf.meta.path, &pf.symbols);
                            }
                            if let Err(e) = tantivy.commit(writer) {
                                warn!("watcher tantivy commit error: {e}");
                            }
                        }
                    }

                    // Phase 5: Update vector embeddings — fully lock-free.
                    // embed_svc, vec_pool are Arcs cloned before the loop.
                    // vec_writer is owned exclusively by this thread.
                    #[cfg(feature = "embed")]
                    if let (Some(ref svc), Some(ref pool), Some(ref writer)) =
                        (&embed_svc, &vec_pool, &vec_writer)
                    {
                        const EMBED_BATCH: usize = 64;

                        // relative-path → raw file bytes for symbol body extraction
                        let content_map: AHashMap<std::path::PathBuf, &[u8]> = to_parse
                            .iter()
                            .map(|(abs, content)| {
                                let rel = abs.strip_prefix(&repo_root).unwrap_or(abs.as_path());
                                (rel.to_owned(), content.as_slice())
                            })
                            .collect();

                        // Fetch symbols with real DB IDs (assigned in phase 3)
                        let mut all_syms = Vec::new();
                        for pf in &parsed {
                            match read_pool.symbols_for_path(&pf.meta.path) {
                                Ok(syms) => all_syms.extend(syms),
                                Err(e) => warn!("embed: symbols_for_path {:?}: {e}", pf.meta.path),
                            }
                        }

                        if !all_syms.is_empty() {
                            let all_ids: Vec<u64> = all_syms.iter().map(|s| s.id).collect();

                            // Lock-free hash check via VecReadPool
                            let existing: AHashMap<u64, [u8; 32]> =
                                pool.existing_hashes(&all_ids).unwrap_or_else(|e| {
                                    warn!("embed: existing_hashes: {e}");
                                    AHashMap::new()
                                });

                            let to_embed: Vec<&recon_core::symbol::Symbol> = all_syms
                                .iter()
                                .filter(|s| existing.get(&s.id).map_or(true, |h| *h != s.body_hash))
                                .collect();

                            if to_embed.is_empty() {
                                debug!(
                                    total = all_syms.len(),
                                    "embed: all symbols unchanged, skipping"
                                );
                            } else {
                                debug!(
                                    changed = to_embed.len(),
                                    total = all_syms.len(),
                                    "embed: processing changed symbols"
                                );

                                for chunk in to_embed.chunks(EMBED_BATCH) {
                                    let texts: Vec<String> = chunk
                                        .iter()
                                        .map(|s| {
                                            let body = content_map
                                                .get(s.path.as_path())
                                                .and_then(|b| b.get(s.byte_range.clone()))
                                                .and_then(|b| std::str::from_utf8(b).ok())
                                                .unwrap_or("");
                                            recon_embed::Embedder::format_symbol(s, body)
                                        })
                                        .collect();

                                    // Channel send — no lock, blocks only for ONNX inference
                                    let vecs = match svc.embed_batch(texts) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            warn!("embed: embed_batch: {e}");
                                            continue;
                                        }
                                    };

                                    let entries: Vec<recon_embed::EmbedEntry> = chunk
                                        .iter()
                                        .zip(vecs)
                                        .map(|(s, vec)| recon_embed::EmbedEntry {
                                            id: s.id,
                                            qualified_name: s.qualified_name.to_string(),
                                            vector: vec,
                                            body_hash: s.body_hash.to_vec(),
                                            lang: s.lang.name().to_string(),
                                        })
                                        .collect();

                                    // Owned writer — zero locking
                                    if let Err(e) = writer.upsert_embeddings(&entries) {
                                        warn!("embed: upsert: {e}");
                                    }
                                }
                            }
                        }
                    }

                    debug!(files = parsed.len(), "watcher batch indexed");

                    // Invalidate all caches — new files/symbols may have been added or changed.
                    cached_paths.store(Arc::new(Vec::new()));
                    cached_file_count.store(0, std::sync::atomic::Ordering::Relaxed);
                    cached_symbols.store(Arc::new(Vec::new()));
                    cached_refs.store(Arc::new(Vec::new()));
                }));

                if result.is_err() {
                    warn!("watcher batch panicked — recovering for next batch");
                }
            }
            info!("file watcher stopped");
        });

        *self.watcher_handle.lock() = Some(handle);
    }

    /// Graceful shutdown: stop the watcher, flush the Tantivy writer, and run
    /// `incremental_vacuum` on SQLite. Safe to call more than once.
    ///
    /// The watcher loop polls `shutdown_flag` every ~500 ms, so the worst-case
    /// latency is one poll interval plus the current batch's processing time.
    /// A final `PRAGMA optimize` still runs from `Store::drop`.
    pub async fn shutdown(&self) {
        info!("shutdown: requested");
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // Wait for the watcher task to exit. Bounded so a wedged batch cannot
        // block shutdown forever.
        let handle_opt = self.watcher_handle.lock().take();
        if let Some(handle) = handle_opt {
            match tokio::time::timeout(Duration::from_secs(10), handle).await {
                Ok(Ok(())) => debug!("shutdown: watcher joined cleanly"),
                Ok(Err(e)) => warn!("shutdown: watcher task error: {e}"),
                Err(_) => warn!("shutdown: watcher did not exit within 10 s — proceeding"),
            }
        }

        // Final Tantivy commit — ensures any uncommitted segments are flushed.
        if let Some(ref mut writer) = *self.tantivy_writer.lock() {
            if let Err(e) = self.tantivy.commit(writer) {
                warn!("shutdown: tantivy commit failed: {e}");
            } else {
                debug!("shutdown: tantivy committed");
            }
        }

        // Reclaim free pages. `PRAGMA optimize` runs from `Store::drop`.
        match self.write_store.lock().incremental_vacuum() {
            Ok(_) => debug!("shutdown: sqlite incremental_vacuum ok"),
            Err(e) => warn!("shutdown: sqlite incremental_vacuum failed: {e}"),
        }

        info!("shutdown: complete");
    }

    /// Resolve a repo-relative path to its canonical absolute form.
    ///
    /// Returns `Err((code, message))` so callers can forward the exact
    /// `ReconErrorCode` into their `tool_error` response.
    fn resolve_path(&self, rel: &str) -> Result<PathBuf, (ReconErrorCode, String)> {
        if redact::is_blocked_path(std::path::Path::new(rel)) {
            return Err((
                ReconErrorCode::PermissionDenied,
                format!("access denied: sensitive file: {rel}"),
            ));
        }
        let path = self.repo_root.join(rel);
        let canonical = path.canonicalize().map_err(|e| {
            (
                ReconErrorCode::NotFound,
                format!("path not found: {rel}: {e}"),
            )
        })?;
        // repo_root is already canonicalized at construction time
        if !canonical.starts_with(&self.repo_root) {
            return Err((
                ReconErrorCode::PathTraversal,
                format!("path traversal denied: {rel}"),
            ));
        }
        Ok(canonical)
    }

    /// Resolve indexed paths to absolute paths — async version with spawn_blocking for git status.
    async fn resolve_search_scope_async(
        &self,
        rel_paths: &[PathBuf],
        filter: Option<&str>,
    ) -> Vec<PathBuf> {
        let filtered: Vec<PathBuf> = match filter {
            Some(f) if !f.is_empty() => {
                let pf = match filters::parse_filter(f) {
                    Ok(pf) => pf,
                    Err(e) => {
                        warn!("filter parse error: {e}");
                        return rel_paths.iter().map(|p| self.repo_root.join(p)).collect();
                    }
                };
                if pf.git_modified_only {
                    let root = self.repo_root.clone();
                    let git_paths = tokio::task::spawn_blocking(move || {
                        recon_indexer::git::status_paths(&root).ok()
                    })
                    .await
                    .ok()
                    .flatten();
                    filters::apply_filter(rel_paths, &pf, git_paths.as_deref())
                } else {
                    filters::apply_filter(rel_paths, &pf, None)
                }
            }
            _ => rel_paths.to_vec(),
        };
        filtered.iter().map(|p| self.repo_root.join(p)).collect()
    }
}

#[tool_router(router = tool_router)]
impl ReconServer {
    #[tool(
        name = "code_outline",
        description = "Show one-line-per-symbol outline of a file. Returns symbol kinds, names, and line numbers in a tree structure. Use instead of Read when you need to understand a file's structure without reading its full content. Typical output: 300-500 tokens for a 500-line file."
    )]
    async fn code_outline(&self, params: Parameters<OutlineParams>) -> String {
        // Validate path doesn't escape repo root
        if let Err((code, msg)) = self.resolve_path(&params.0.path) {
            return tool_error(
                code,
                msg,
                Some(serde_json::json!({ "path": params.0.path })),
            );
        }
        let symbols = {
            let rel_path = PathBuf::from(&params.0.path);
            match self.read_pool.symbols_for_path(&rel_path) {
                Ok(s) => s,
                Err(e) => return tool_error_from(&e),
            }
        };

        // O(n) child lookup: build parent_id -> children map in one pass
        let mut children_map: AHashMap<u64, Vec<&recon_core::symbol::Symbol>> = AHashMap::new();
        for sym in &symbols {
            if let Some(pid) = sym.parent_id {
                children_map.entry(pid).or_default().push(sym);
            }
        }

        let mut entries = SmallVec::new();
        for sym in &symbols {
            if sym.parent_id.is_none() {
                let children = children_map
                    .get(&sym.id)
                    .map(|kids| {
                        kids.iter()
                            .map(|c| OutlineEntry {
                                kind: c.kind,
                                name: c.name.clone(),
                                line: *c.line_range.start(),
                                children: vec![],
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                entries.push(OutlineEntry {
                    kind: sym.kind,
                    name: sym.name.clone(),
                    line: *sym.line_range.start(),
                    children,
                });
            }
        }

        let rel_path = PathBuf::from(&params.0.path);
        let view = ToolOutput::Outline(OutlineView {
            path: rel_path,
            entries,
        });
        redact_response(serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")))
    }

    #[tool(
        name = "code_skeleton",
        description = "Show signatures and docstrings with bodies elided as '...'. 10x compression vs full file read. Use instead of Read when you need to understand APIs and structure. Output: ~300 tokens per 3000-token file."
    )]
    async fn code_skeleton(&self, params: Parameters<SkeletonParams>) -> String {
        let rel_path = PathBuf::from(&params.0.path);
        let symbols = self
            .read_pool
            .symbols_for_path(&rel_path)
            .unwrap_or_default();

        let mut skeleton = String::with_capacity(symbols.len() * 80);
        for sym in &symbols {
            if sym.parent_id.is_some() && params.0.depth < 2 {
                continue;
            }
            if let Some(doc) = &sym.doc {
                for line in doc.lines() {
                    skeleton.push_str(line);
                    skeleton.push('\n');
                }
            }
            if let Some(sig) = &sym.signature {
                skeleton.push_str(sig);
            } else {
                skeleton.push_str(sym.kind.label());
                skeleton.push(' ');
                skeleton.push_str(&sym.name);
            }
            skeleton.push_str(" { ... }\n\n");
        }

        if skeleton.is_empty() {
            let abs_path = match self.resolve_path(&params.0.path) {
                Ok(p) => p,
                Err((code, msg)) => {
                    return tool_error(
                        code,
                        msg,
                        Some(serde_json::json!({ "path": params.0.path })),
                    );
                }
            };
            // Size cap to prevent OOM on large files (minified bundles, lock files, etc.)
            match tokio::fs::metadata(&abs_path).await {
                Ok(m) if m.len() > MAX_READ_FILE_SIZE => {
                    return tool_error(
                        ReconErrorCode::FileTooLarge,
                        format!(
                            "file too large ({} MB, max {} MB)",
                            m.len() / (1024 * 1024),
                            MAX_READ_FILE_SIZE / (1024 * 1024)
                        ),
                        Some(serde_json::json!({
                            "path": params.0.path,
                            "size_bytes": m.len(),
                            "max_bytes": MAX_READ_FILE_SIZE,
                        })),
                    );
                }
                Err(e) => {
                    return tool_error(
                        ReconErrorCode::Io,
                        format!("reading file metadata: {e}"),
                        Some(serde_json::json!({ "path": params.0.path })),
                    );
                }
                _ => {}
            }
            let content = match tokio::fs::read_to_string(&abs_path).await {
                Ok(c) => c,
                Err(e) => {
                    return tool_error(
                        ReconErrorCode::Io,
                        format!("reading file: {e}"),
                        Some(serde_json::json!({ "path": params.0.path })),
                    );
                }
            };
            skeleton = content.lines().take(50).collect::<Vec<_>>().join("\n");
        }

        let token_est = recon_search::tokens::estimate_tokens(&skeleton);
        let view = ToolOutput::Skeleton(SkeletonView {
            path: Some(rel_path),
            content: skeleton,
            token_estimate: token_est,
        });
        redact_response(serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")))
    }

    #[tool(
        name = "code_read_symbol",
        description = "Read the full source of one symbol plus its parent chain and caller/callee references. Use instead of Read when you need one specific function or type. Output: ~200-800 tokens."
    )]
    async fn code_read_symbol(&self, params: Parameters<ReadSymbolParams>) -> String {
        let abs_path = match self.resolve_path(&params.0.path) {
            Ok(p) => p,
            Err((code, msg)) => {
                return tool_error(
                    code,
                    msg,
                    Some(serde_json::json!({ "path": params.0.path })),
                );
            }
        };
        // Size cap to prevent OOM on large files.
        match tokio::fs::metadata(&abs_path).await {
            Ok(m) if m.len() > MAX_READ_FILE_SIZE => {
                return tool_error(
                    ReconErrorCode::FileTooLarge,
                    format!(
                        "file too large ({} MB, max {} MB)",
                        m.len() / (1024 * 1024),
                        MAX_READ_FILE_SIZE / (1024 * 1024)
                    ),
                    Some(serde_json::json!({
                        "path": params.0.path,
                        "size_bytes": m.len(),
                        "max_bytes": MAX_READ_FILE_SIZE,
                    })),
                );
            }
            Err(e) => {
                return tool_error(
                    ReconErrorCode::Io,
                    format!("reading file metadata: {e}"),
                    Some(serde_json::json!({ "path": params.0.path })),
                );
            }
            _ => {}
        }
        let content = match tokio::fs::read_to_string(&abs_path).await {
            Ok(c) => c,
            Err(e) => {
                return tool_error(
                    ReconErrorCode::Io,
                    format!("reading file: {e}"),
                    Some(serde_json::json!({ "path": params.0.path })),
                );
            }
        };

        let rel_path = PathBuf::from(&params.0.path);
        let symbols = self
            .read_pool
            .symbols_for_path(&rel_path)
            .unwrap_or_default();

        let target = if let Ok(line) = params.0.symbol_or_line.parse::<u32>() {
            symbols.iter().find(|s| s.line_range.contains(&line))
        } else {
            symbols
                .iter()
                .find(|s| s.name.as_str() == params.0.symbol_or_line)
        };

        let sym = match target {
            Some(s) => s,
            None => {
                return tool_error(
                    ReconErrorCode::NotFound,
                    format!("symbol not found: {}", params.0.symbol_or_line),
                    Some(serde_json::json!({
                        "path": params.0.path,
                        "symbol_or_line": params.0.symbol_or_line,
                    })),
                );
            }
        };

        let body = content
            .get(sym.byte_range.clone())
            .unwrap_or("[byte range out of bounds]")
            .to_string();

        // Extract doc from source file: comment block immediately before symbol
        let doc = extract_doc_from_source(&content, sym.byte_range.start);

        let refs = self
            .read_pool
            .refs_for_ident(sym.name.as_str())
            .unwrap_or_default();
        let callers: Vec<RefEntry> = refs
            .iter()
            .take(10)
            .map(|r| RefEntry {
                path: (*r.src_path).clone(),
                line: 0,
                col: None,
                snippet: r.ident.clone(),
                enclosing_symbol: None,
            })
            .collect();

        // Build parent chain: walk up parent_id to root
        let mut parent_chain: Vec<String> = Vec::new();
        let mut current_parent = sym.parent_id;
        while let Some(parent_id) = current_parent {
            if let Some(parent) = symbols.iter().find(|s| s.id == parent_id) {
                parent_chain.push(format!(
                    "{}:{} {}",
                    parent.kind.label(),
                    parent.line_range.start(),
                    parent.qualified_name
                ));
                current_parent = parent.parent_id;
            } else {
                break;
            }
        }
        parent_chain.reverse();

        // Build callees: symbols this symbol references
        let callees: Vec<RefEntry> = refs
            .iter()
            .filter(|r| r.src_symbol_id == sym.id)
            .map(|r| RefEntry {
                path: (*r.src_path).clone(),
                line: 0,
                col: None,
                snippet: r.ident.clone(),
                enclosing_symbol: None,
            })
            .collect();

        let view = ToolOutput::SymbolCard(SymbolCardView {
            path: rel_path,
            qualified_name: sym.qualified_name.to_string(),
            kind: sym.kind,
            signature: sym.signature.as_deref().map(str::to_owned),
            doc,
            body,
            line_range: (*sym.line_range.start(), *sym.line_range.end()),
            parent_chain,
            callers,
            callees,
        });
        redact_response(serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")))
    }

    #[tool(
        name = "code_find_symbol",
        description = "Find symbols by name across the codebase. Tiered: exact SQLite match -> Tantivy BM25 -> FTS5 trigram + nucleo fuzzy. Use instead of Grep when searching for functions, types, or classes."
    )]
    async fn code_find_symbol(&self, params: Parameters<FindSymbolParams>) -> String {
        // All reads go through lock-free ReadPool
        // Tier 0: exact match via SQLite index
        let mut results = self
            .read_pool
            .find_symbols_exact(&params.0.name, 20)
            .unwrap_or_default();

        // Tier 1: Tantivy BM25 structured search (Tantivy is already lock-free)
        if results.is_empty() {
            let hits = self.tantivy.search(&params.0.name, 20).unwrap_or_default();
            for hit in &hits {
                if let Some(sym) = self
                    .read_pool
                    .get_symbol_by_qname(&hit.qualified_name)
                    .ok()
                    .flatten()
                {
                    results.push(sym);
                }
            }
        }

        // Tier 2: FTS5 trigram + nucleo fuzzy rescore
        if results.is_empty() {
            let fts_results = self
                .read_pool
                .search_symbols_fuzzy(&params.0.name, 50)
                .unwrap_or_default();
            let ranked = fuzzy::fuzzy_rank(&fts_results, &params.0.name, 20);
            results = ranked
                .into_iter()
                .map(|(i, _): (usize, _)| fts_results[i].clone())
                .collect();
        }

        // Tier 3: Semantic embedding fallback (feature-gated)
        #[allow(unused_mut)]
        let mut from_embedding = false;
        #[cfg(feature = "embed")]
        if results.is_empty() {
            let svc = self.embed_service.lock().clone();
            let pool = self.vec_read_pool.lock().clone();
            if let (Some(svc), Some(pool)) = (svc, pool) {
                let query = params.0.name.clone();
                let query_vec =
                    match tokio::task::spawn_blocking(move || svc.embed_one(&query)).await {
                        Ok(Ok(v)) => v,
                        Ok(Err(e)) => {
                            warn!("code_find_symbol embed_one error: {e}");
                            Vec::new()
                        }
                        Err(e) => {
                            warn!("code_find_symbol embed task join error: {e}");
                            Vec::new()
                        }
                    };
                if !query_vec.is_empty() {
                    if let Ok(vec_results) = pool.search(query_vec, None, 20) {
                        for (id, _dist) in vec_results {
                            if let Ok(Some(sym)) = self.read_pool.symbol_by_id(id) {
                                results.push(sym);
                            }
                        }
                        if !results.is_empty() {
                            from_embedding = true;
                        }
                    }
                }
            }
        }

        // Apply filters
        if let Some(kind_filter) = &params.0.kind {
            results.retain(|s| s.kind.label() == kind_filter.as_str());
        }
        if let Some(lang_filter) = &params.0.lang {
            let lang = Language::from_extension(lang_filter);
            if lang != Language::Unknown {
                results.retain(|s| s.lang == lang);
            }
        }

        let source = if from_embedding {
            "semantic"
        } else {
            "lexical"
        };
        let entries: Vec<serde_json::Value> = results
            .iter()
            .map(|s| {
                serde_json::json!({
                    "qualified_name": s.qualified_name.as_str(),
                    "path": s.path.to_string_lossy(),
                    "line": *s.line_range.start(),
                    "kind": s.kind.label(),
                    "signature": s.signature,
                    "source": source,
                })
            })
            .collect();

        redact_response(serde_json::to_string(&entries).unwrap_or_else(|e| format!("Error: {e}")))
    }

    #[tool(
        name = "code_find_refs",
        description = "Find all references to a symbol. Returns a count and top-k call sites as path:line triples. Use instead of Grep for finding usages of a function or type."
    )]
    async fn code_find_refs(&self, params: Parameters<FindRefsParams>) -> String {
        let refs = self
            .read_pool
            .refs_for_ident(&params.0.symbol)
            .unwrap_or_default();

        // Collect unique src_symbol_ids and fetch only their locations — not all 80K symbols.
        let unique_ids: Vec<u64> = refs
            .iter()
            .map(|r| r.src_symbol_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let locations = self
            .read_pool
            .symbol_locations_by_ids(&unique_ids)
            .unwrap_or_default();
        let loc_map: ahash::AHashMap<u64, (String, u32)> = locations
            .into_iter()
            .map(|(id, path, line)| (id, (path, line)))
            .collect();

        let top_k: Vec<RefEntry> = refs
            .iter()
            .take(20)
            .map(|r| {
                let (path, line) = loc_map
                    .get(&r.src_symbol_id)
                    .cloned()
                    .unwrap_or_else(|| (String::new(), 0));
                RefEntry {
                    path: PathBuf::from(path),
                    line,
                    col: None,
                    snippet: r.ident.clone(),
                    enclosing_symbol: None,
                }
            })
            .collect();

        let view = ToolOutput::ReferenceDigest(RefDigestView {
            symbol: params.0.symbol.as_str().into(),
            total: refs.len(),
            top_k,
        });
        redact_response(serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")))
    }

    #[tool(
        name = "code_search",
        description = "Search for text patterns. Modes: exact (default), regex, hybrid (BM25 + text fused via reciprocal rank fusion). Use instead of Grep for code search."
    )]
    async fn code_search(&self, params: Parameters<SearchParams>) -> String {
        let paths = self.cached_file_paths();

        let abs_paths = self
            .resolve_search_scope_async(&paths, params.0.filter.as_deref())
            .await;

        // Semantic mode — fully lock-free via EmbedService channel + VecReadPool.
        #[cfg(feature = "embed")]
        if params.0.mode == "semantic" {
            let svc = self.embed_service.lock().clone();
            let pool = self.vec_read_pool.lock().clone();
            match (svc, pool) {
                (Some(svc), Some(pool)) => {
                    // embed_one blocks the caller waiting on the ONNX worker thread;
                    // use spawn_blocking so we don't stall the tokio executor.
                    let query = params.0.query.clone();
                    let query_vec = match tokio::task::spawn_blocking(move || svc.embed_one(&query))
                        .await
                    {
                        Ok(Ok(v)) => v,
                        Ok(Err(e)) => return format!("embed error: {e}"),
                        Err(e) => return format!("embed task join error: {e}"),
                    };
                    let results = match pool.search(query_vec, None, 20) {
                        Ok(r) => r,
                        Err(e) => return format!("vector search error: {e}"),
                    };
                    let entries: Vec<serde_json::Value> = results
                        .iter()
                        .map(|(id, dist)| serde_json::json!({"symbol_id": id, "distance": dist}))
                        .collect();
                    return redact_response(
                        serde_json::to_string(&entries)
                            .unwrap_or_else(|e| format!("Error: {e}")),
                    );
                }
                _ => {
                    return "semantic search requires embed feature to be initialized (run init_embed)".into()
                }
            }
        }
        #[cfg(not(feature = "embed"))]
        if params.0.mode == "semantic" {
            return "semantic search requires the 'embed' feature flag".into();
        }

        if params.0.mode == "hybrid" {
            // RRF fusion: Tantivy BM25 results + text grep results
            let tantivy_hits = self.tantivy.search(&params.0.query, 20).unwrap_or_default();
            let q = TextQuery {
                pattern: params.0.query.clone(),
                is_regex: false,
                max_results: 20,
                scope: abs_paths.clone(),
            };
            let text_hits = self.text_searcher.search(&q).unwrap_or_default();

            let mut rrf: ahash::AHashMap<String, (f64, serde_json::Value)> = ahash::AHashMap::new();
            let k = 60.0;

            for (rank, hit) in tantivy_hits.iter().enumerate() {
                let key = format!("{}:{}", hit.path, hit.name);
                let score = 1.0 / (k + rank as f64 + 1.0);
                rrf.entry(key)
                    .or_insert_with(|| {
                        (
                            0.0,
                            serde_json::json!({
                                "path": hit.path, "name": hit.name, "kind": hit.kind,
                                "signature": hit.signature, "source": "tantivy",
                            }),
                        )
                    })
                    .0 += score;
            }
            for (rank, hit) in text_hits.iter().enumerate() {
                let rel = hit.path.strip_prefix(&self.repo_root).unwrap_or(&hit.path);
                let key = format!("{}:{}", rel.to_string_lossy(), hit.line);
                let score = 1.0 / (k + rank as f64 + 1.0);
                rrf.entry(key)
                    .or_insert_with(|| {
                        (
                            0.0,
                            serde_json::json!({
                                "path": rel.to_string_lossy(), "line": hit.line,
                                "text": hit.line_text, "source": "text",
                            }),
                        )
                    })
                    .0 += score;
            }

            let mut sorted: Vec<_> = rrf.into_values().collect();
            sorted.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let entries: Vec<serde_json::Value> =
                sorted.into_iter().take(30).map(|(_, v)| v).collect();

            return redact_response(
                serde_json::to_string(&entries).unwrap_or_else(|e| format!("Error: {e}")),
            );
        }

        if params.0.mode == "regex" {
            let q = TextQuery {
                pattern: params.0.query.clone(),
                is_regex: true,
                max_results: 30,
                scope: abs_paths,
            };
            let hits = self.text_searcher.search(&q).unwrap_or_default();

            let entries: Vec<serde_json::Value> = hits
                .iter()
                .map(|h| {
                    let rel = h.path.strip_prefix(&self.repo_root).unwrap_or(&h.path);
                    serde_json::json!({ "path": rel.to_string_lossy(), "line": h.line, "col": h.col, "text": h.line_text })
                })
                .collect();

            return redact_response(
                serde_json::to_string(&entries).unwrap_or_else(|e| format!("Error: {e}")),
            );
        }

        // Exact mode: try Tantivy first (sub-ms), fall back to grep only if empty
        let tantivy_hits = self.tantivy.search(&params.0.query, 30).unwrap_or_default();
        if !tantivy_hits.is_empty() {
            let entries: Vec<serde_json::Value> = tantivy_hits
                .iter()
                .map(|hit| {
                    serde_json::json!({
                        "path": hit.path, "line": 0, "col": null,
                        "text": hit.signature.as_deref().unwrap_or(&hit.name),
                    })
                })
                .collect();
            return redact_response(
                serde_json::to_string(&entries).unwrap_or_else(|e| format!("Error: {e}")),
            );
        }

        // Tantivy had no hits — fall back to text grep
        let q = TextQuery {
            pattern: params.0.query.clone(),
            is_regex: false,
            max_results: 30,
            scope: abs_paths,
        };
        let hits = self.text_searcher.search(&q).unwrap_or_default();

        let entries: Vec<serde_json::Value> = hits
            .iter()
            .map(|h| {
                let rel = h.path.strip_prefix(&self.repo_root).unwrap_or(&h.path);
                serde_json::json!({ "path": rel.to_string_lossy(), "line": h.line, "col": h.col, "text": h.line_text })
            })
            .collect();

        redact_response(serde_json::to_string(&entries).unwrap_or_else(|e| format!("Error: {e}")))
    }

    #[tool(
        name = "code_list",
        description = "List indexed source files with language, line count, and top symbols. Use instead of Glob when you need structured file listings. Supports language filter."
    )]
    async fn code_list(&self, params: Parameters<ListParams>) -> String {
        // Single query for all files + symbol counts + top symbols
        let summaries = self.read_pool.file_symbol_summaries().unwrap_or_default();

        // Apply filters in memory
        let filter_parsed = params
            .0
            .filter
            .as_deref()
            .filter(|f| !f.is_empty())
            .and_then(|f| filters::parse_filter(f).ok());

        // Resolve git-modified paths if needed (for code_list paths are relative)
        let git_paths = filter_parsed.as_ref().and_then(|pf| {
            if pf.git_modified_only {
                recon_indexer::git::status_paths(&self.repo_root)
                    .ok()
                    .map(|abs_paths| {
                        abs_paths
                            .into_iter()
                            .filter_map(|p| p.strip_prefix(&self.repo_root).ok().map(PathBuf::from))
                            .collect::<Vec<_>>()
                    })
            } else {
                None
            }
        });

        let mut entries: Vec<serde_json::Value> = Vec::with_capacity(summaries.len());
        for (path, sym_count, top_syms) in &summaries {
            if let Some(ref pf) = filter_parsed {
                if filters::apply_filter(std::slice::from_ref(path), pf, git_paths.as_deref())
                    .is_empty()
                {
                    continue;
                }
            }
            let lang = Language::from_path(path);
            if let Some(lang_filter) = &params.0.lang {
                let filter_lang = Language::from_extension(lang_filter);
                if filter_lang != Language::Unknown && lang != filter_lang {
                    continue;
                }
            }
            if let Some(glob_pat) = &params.0.glob {
                let path_str = path.to_string_lossy();
                if !path_str.contains(glob_pat.trim_matches('*')) {
                    continue;
                }
            }

            entries.push(serde_json::json!({
                "path": path.to_string_lossy(), "lang": lang.name(),
                "symbol_count": sym_count, "top_symbols": top_syms,
            }));
        }

        redact_response(serde_json::to_string(&entries).unwrap_or_else(|e| format!("Error: {e}")))
    }

    #[tool(
        name = "code_repo_map",
        description = "Generate a ranked overview of the most important symbols in the repo. Uses personalized PageRank over the reference graph with Aider-style edge weights. Output fits within a token budget (default 2000). Best first tool to call for orientation."
    )]
    async fn code_repo_map(&self, params: Parameters<RepoMapParams>) -> String {
        let focus_files = params.0.focus_files.as_deref().unwrap_or(&[]);
        let budget = params.0.token_budget;

        // All reads go through lock-free cached accessors
        let (all_symbols, all_refs, cache_key) = {
            // Check cache for unfocused maps
            if focus_files.is_empty() {
                let last_idx = self.read_pool.max_indexed_at().unwrap_or(0);
                let key = format!("map_cache:{}:{}", last_idx, budget);
                if let Ok(Some(cached)) = self.read_pool.get_meta(&key) {
                    return cached;
                }
                let syms = self.cached_all_symbols();
                let refs = self.cached_all_refs();
                (syms, refs, Some(key))
            } else {
                let syms = self.cached_all_symbols();
                let refs = self.cached_all_refs();
                (syms, refs, None)
            }
        };

        // Compute focus indices if focused
        let focus_indices: Vec<usize> = if !focus_files.is_empty() {
            let focus_set: std::collections::HashSet<&str> =
                focus_files.iter().map(|s| s.as_str()).collect();
            all_symbols
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    let p = s.path.to_string_lossy();
                    focus_set.iter().any(|f| p.contains(f))
                })
                .map(|(i, _)| i)
                .collect()
        } else {
            vec![]
        };

        let ranked = pagerank::pagerank(
            &all_symbols,
            &all_refs,
            &focus_indices,
            0.85,
            pagerank::DEFAULT_MAX_ITERATIONS,
        );
        let content = pagerank::render_repo_map(&all_symbols, &ranked, budget);

        let token_est = recon_search::tokens::estimate_tokens(&content);
        let view = ToolOutput::Skeleton(SkeletonView {
            path: None,
            content,
            token_estimate: token_est,
        });
        let result =
            redact_response(serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")));

        // Cache unfocused results (write lock only for cache update)
        if let Some(key) = cache_key {
            let store = self.write_store.lock();
            if let Err(e) = store.delete_meta_prefix("map_cache:") {
                warn!("failed to clear map cache: {e}");
            }
            if let Err(e) = store.set_meta(&key, &result) {
                warn!("failed to write map cache: {e}");
            }
        }

        result
    }

    #[tool(
        name = "code_find_strings",
        description = "Search for patterns in string literals and comments. Finds SQL fragments, i18n keys, log messages that structural search misses."
    )]
    async fn code_find_strings(&self, params: Parameters<FindStringsParams>) -> String {
        let paths = self.cached_file_paths();

        let abs_paths = self
            .resolve_search_scope_async(&paths, params.0.filter.as_deref())
            .await;
        let q = TextQuery {
            pattern: params.0.pattern.clone(),
            is_regex: false,
            max_results: 30,
            scope: abs_paths,
        };
        let hits = self.text_searcher.search(&q).unwrap_or_default();

        let entries: Vec<serde_json::Value> = hits
            .iter()
            .map(|h| {
                let rel = h.path.strip_prefix(&self.repo_root).unwrap_or(&h.path);
                serde_json::json!({ "path": rel.to_string_lossy(), "line": h.line, "text": h.line_text, "kind": params.0.kind })
            })
            .collect();

        redact_response(serde_json::to_string(&entries).unwrap_or_else(|e| format!("Error: {e}")))
    }

    #[tool(
        name = "code_multi_find",
        description = "Search for multiple patterns at once. More efficient than multiple code_search calls. Returns results grouped by pattern."
    )]
    async fn code_multi_find(&self, params: Parameters<MultiFindParams>) -> String {
        let paths = self.cached_file_paths();

        let abs_paths = self
            .resolve_search_scope_async(&paths, params.0.filter.as_deref())
            .await;
        let pat_refs: Vec<&str> = params.0.patterns.iter().map(|s| s.as_str()).collect();
        let multi_results = self
            .text_searcher
            .multi_search(&pat_refs, &abs_paths, 10)
            .unwrap_or_default();

        let results: Vec<serde_json::Value> = multi_results
            .iter()
            .map(|(pattern, hits)| {
                let entries: Vec<serde_json::Value> = hits
                    .iter()
                    .map(|h| {
                        let rel = h.path.strip_prefix(&self.repo_root).unwrap_or(&h.path);
                        serde_json::json!({ "path": rel.to_string_lossy(), "line": h.line, "text": h.line_text })
                    })
                    .collect();
                serde_json::json!({ "pattern": pattern, "hits": entries })
            })
            .collect();

        redact_response(serde_json::to_string(&results).unwrap_or_else(|e| format!("Error: {e}")))
    }

    #[tool(
        name = "code_reindex",
        description = "Trigger a full re-index of the repository. Use when you suspect the index is stale or after major file changes outside the editor."
    )]
    async fn code_reindex(&self, params: Parameters<ReindexParams>) -> String {
        let force = params.0.force;

        // Clear cache under short write lock
        {
            let store = self.write_store.lock();
            let _ = store.delete_meta_prefix("map_cache:");
        }

        let write_store = self.write_store.clone();
        let tantivy = self.tantivy.clone();
        let tantivy_writer = self.tantivy_writer.clone();
        let repo_root = self.repo_root.clone();

        // Heavy work runs on a blocking thread — parse locklessly, write in chunks
        let result = tokio::task::spawn_blocking(move || {
            use recon_indexer::indexer;
            use recon_indexer::walker;

            if force {
                // Full reindex: clear existing data first
                info!("force reindex: clearing existing data");
                {
                    let store = write_store.lock();
                    let all_paths = store.all_file_paths().unwrap_or_default();
                    for path in &all_paths {
                        let _ = store.delete_file_cascade(path);
                    }
                    if let Some(ref mut writer) = tantivy_writer.lock().as_mut() {
                        let _ = tantivy.commit(writer);
                    }
                }

                // Full walk + parse (force path)
                let paths = walker::walk_repo(&repo_root);
                let pools =
                    std::sync::Arc::new(LanguagePools::new(rayon::current_num_threads().max(4)));
                let parsed: Vec<indexer::ParsedFile> = paths
                    .par_iter()
                    .filter_map(|path| {
                        let content = std::fs::read(path).ok()?;
                        if walker::is_generated_content(&content) {
                            return None;
                        }
                        let hash = recon_storage::hash::blake3_bytes(&content);
                        let mtime = indexer::mtime_of(path);
                        indexer::parse_file_with_content(
                            &content, path, &repo_root, &pools, hash, mtime,
                        )
                    })
                    .collect();

                let mut files_indexed = 0usize;
                let mut errors = 0usize;
                const CHUNK_SIZE: usize = 500;

                for chunk in parsed.chunks(CHUNK_SIZE) {
                    let bulk: Vec<_> = chunk
                        .iter()
                        .map(|p| (&p.meta, p.symbols.as_slice(), p.refs.as_slice()))
                        .collect();
                    let store = write_store.lock();
                    match store.batch_index_files(&bulk) {
                        Ok(()) => files_indexed += chunk.len(),
                        Err(e) => {
                            warn!(chunk_size = chunk.len(), "reindex store error: {e}");
                            errors += chunk.len();
                        }
                    }
                }

                // Tantivy indexing
                {
                    let mut tw = tantivy_writer.lock();
                    if let Some(ref mut writer) = *tw {
                        let mut docs = 0usize;
                        for pf in &parsed {
                            let _ = tantivy.index_symbols(writer, &pf.meta.path, &pf.symbols);
                            docs += pf.symbols.len();
                            if docs >= 20_000 {
                                let _ = tantivy.commit(writer);
                                docs = 0;
                            }
                        }
                        let _ = tantivy.commit(writer);
                    }
                }

                let total_symbols = write_store.lock().symbol_count().unwrap_or(0);

                serde_json::json!({
                    "status": "ok",
                    "files_indexed": files_indexed,
                    "total_symbols": total_symbols,
                    "errors": errors,
                    "force": true,
                })
            } else {
                // Incremental reindex: use git diff (or Merkle fallback) to only re-parse changed files
                let store = write_store.lock();
                let result = indexer::index_repo_incremental(
                    &store,
                    Some(&tantivy),
                    &repo_root,
                    tantivy_writer.lock().as_mut(),
                );
                match result {
                    Ok(stats) => {
                        serde_json::json!({
                            "status": "ok",
                            "files_indexed": stats.files_indexed,
                            "total_symbols": stats.total_symbols,
                            "errors": stats.errors,
                            "force": false,
                        })
                    }
                    Err(e) => {
                        serde_json::json!({
                            "status": "error",
                            "error": format!("{e}"),
                            "force": false,
                        })
                    }
                }
            }
        })
        .await;

        match result {
            Ok(stats) => {
                // Refresh path cache after reindex.
                self.refresh_caches();
                serde_json::to_string(&stats).unwrap_or_else(|e| format!("Error: {e}"))
            }
            Err(e) => format!("Reindex failed: {e}"),
        }
    }

    #[tool(
        name = "code_stats",
        description = "Report index health: total files, symbols, last indexed time, Tantivy doc count. Use to check if the index is fresh and complete."
    )]
    async fn code_stats(&self, _params: Parameters<StatsParams>) -> String {
        let mut file_count = self.cached_file_count.load(Ordering::Relaxed);
        if file_count == 0 {
            // Cache cold — get actual count.
            file_count = self.read_pool.file_count().unwrap_or(0);
            self.cached_file_count.store(file_count, Ordering::Relaxed);
        }
        let symbol_count = self.read_pool.symbol_count().unwrap_or(0);
        let tantivy_docs = self.tantivy.doc_count();
        let schema_version = self
            .read_pool
            .get_meta("schema_version")
            .unwrap_or(None)
            .unwrap_or_default();

        redact_response(
            serde_json::to_string(&serde_json::json!({
                "files_indexed": file_count,
                "total_symbols": symbol_count,
                "tantivy_docs": tantivy_docs,
                "schema_version": schema_version,
                "repo_root": self.repo_root.to_string_lossy(),
            }))
            .unwrap_or_else(|e| format!("Error: {e}")),
        )
    }
}

#[tool_handler]
impl ServerHandler for ReconServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new("recon", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "recon is a code intelligence MCP server. \
             Prefer code_* tools over Read/Grep/Glob: \
             code_outline for file structure, \
             code_skeleton for API overview (10x compression), \
             code_find_symbol for symbol search (3-tier: exact/BM25/fuzzy), \
             code_search for text patterns (supports filter DSL), \
             code_repo_map for orientation (PageRank-ranked overview). \
             These tools return structured, token-efficient results. \
             Use Read only when you need the exact source of a specific symbol \
             (prefer code_read_symbol for that)."
                .to_string(),
        )
    }
}

/// Extract documentation comment from source code immediately before a symbol.
/// Handles Rust (///, /** */), Python (""", '''), and Go (//) doc comments.
fn extract_doc_from_source(content: &str, byte_start: usize) -> Option<String> {
    let before = content.get(..byte_start)?;
    let lines: Vec<&str> = before.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut doc_lines: Vec<&str> = Vec::new();
    let mut i = lines.len();

    // Skip blank lines before the symbol
    while i > 0 && lines[i - 1].trim().is_empty() {
        i -= 1;
    }

    // Collect doc comment lines (working backwards)
    while i > 0 {
        let line = lines[i - 1].trim();
        if line.starts_with("///") {
            doc_lines.push(line.strip_prefix("///").unwrap_or(line).trim());
            i -= 1;
        } else if line.starts_with("//") {
            doc_lines.push(line.strip_prefix("//").unwrap_or(line).trim());
            i -= 1;
        } else if line.starts_with('#') && line.contains('"') {
            // Python docstring or decorator — stop at decorator
            if line.starts_with("#[") {
                break;
            }
            doc_lines.push(line.trim_start_matches('#').trim().trim_matches('"'));
            i -= 1;
        } else if line == "\"\"\"" || line == "'''" {
            // End of Python multi-line docstring
            i -= 1;
            // Collect until opening """
            while i > 0 {
                let inner = lines[i - 1].trim();
                if inner.ends_with("\"\"\"") || inner.ends_with("'''") {
                    doc_lines.push(
                        inner
                            .trim_end_matches("\"\"\"")
                            .trim_end_matches("'''")
                            .trim(),
                    );
                    i -= 1;
                    break;
                }
                doc_lines.push(inner);
                i -= 1;
            }
        } else {
            break;
        }
    }

    if doc_lines.is_empty() {
        return None;
    }

    doc_lines.reverse();
    Some(
        doc_lines
            .iter()
            .map(|s| s.trim())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use recon_search::tantivy_backend::TantivyBackend;
    use recon_storage::store::Store;

    fn make_test_server() -> ReconServer {
        let store = Store::open_memory().unwrap();
        let tantivy = TantivyBackend::open_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        ReconServer::new(tmp.path().to_path_buf(), store, tantivy).unwrap()
    }

    /// Helper: create a temp repo with known source files and index it.
    /// Returns (server, temp_dir) so the temp dir stays alive for the test.
    async fn make_indexed_server() -> (ReconServer, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create a small multi-file project
        fs_write(root.join("src/lib.rs"), "pub mod math;\npub mod utils;\n");
        fs_write(
            root.join("src/math.rs"),
            "/// Add two numbers together.\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n/// Multiply two numbers.\npub fn mul(a: i32, b: i32) -> i32 {\n    a * b\n}\n\nfn internal_helper(x: i32) -> i32 {\n    x * 2\n}\n",
        );
        fs_write(
            root.join("src/utils.rs"),
            "use crate::math::add;\n\npub fn sum_three(a: i32, b: i32, c: i32) -> i32 {\n    add(add(a, b), c)\n}\n",
        );

        // Use an on-disk store so the read pool shares data with the write store
        let db_path = root.join(".recon").join("recon.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let store = Store::open(&db_path).unwrap();
        let tantivy_dir = root.join(".recon").join("tantivy");
        std::fs::create_dir_all(&tantivy_dir).unwrap();
        let tantivy = TantivyBackend::open(&tantivy_dir).unwrap();
        let server = ReconServer::new(root.to_path_buf(), store, tantivy).unwrap();
        server.index_repo().await.unwrap();
        (server, tmp)
    }

    fn fs_write(path: std::path::PathBuf, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn server_new_does_not_panic() {
        let _server = make_test_server();
    }

    #[test]
    fn server_new_returns_result() {
        let store = Store::open_memory().unwrap();
        let tantivy = TantivyBackend::open_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let result = ReconServer::new(tmp.path().to_path_buf(), store, tantivy);
        assert!(
            result.is_ok(),
            "Server::new should succeed for a valid setup"
        );
    }

    #[tokio::test]
    async fn code_outline_empty_repo() {
        let server = make_test_server();
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::OutlineParams {
            path: "nonexistent.rs".into(),
        });
        let result = server.code_outline(params).await;
        assert!(!result.is_empty());
    }

    #[tokio::test]
    async fn code_repo_map_empty_returns_string() {
        let server = make_test_server();
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::RepoMapParams {
            focus_files: None,
            token_budget: 500,
        });
        let result = server.code_repo_map(params).await;
        assert!(!result.is_empty());
    }

    #[tokio::test]
    async fn code_outline_indexed_file() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::OutlineParams {
            path: "src/math.rs".into(),
        });
        let result = server.code_outline(params).await;
        assert!(
            !result.starts_with("Error:"),
            "code_outline should succeed for indexed file: {result}"
        );
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("Outline"));
        let entries = &json["entries"];
        assert!(
            entries
                .as_array()
                .is_some_and(|a| a.iter().any(|e| e["name"] == "add")),
            "should contain 'add' function"
        );
        assert!(
            entries
                .as_array()
                .is_some_and(|a| a.iter().any(|e| e["name"] == "mul")),
            "should contain 'mul' function"
        );
    }

    #[tokio::test]
    async fn code_read_symbol_by_name() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::ReadSymbolParams {
            path: "src/math.rs".into(),
            symbol_or_line: "add".into(),
        });
        let result = server.code_read_symbol(params).await;
        assert!(
            !result.starts_with("Error:"),
            "code_read_symbol should succeed: {result}"
        );
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("SymbolCard"));
        assert_eq!(json["qualified_name"].as_str(), Some("add"));
        assert!(json["body"].as_str().is_some_and(|b| b.contains("a + b")));
        assert!(json["doc"]
            .as_str()
            .is_some_and(|d| d.contains("Add two numbers")));
    }

    #[tokio::test]
    async fn code_read_symbol_by_line_number() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        // Line 2 is inside the `add` function body
        let params = Parameters(crate::tools::ReadSymbolParams {
            path: "src/math.rs".into(),
            symbol_or_line: "2".into(),
        });
        let result = server.code_read_symbol(params).await;
        assert!(
            !result.starts_with("Error:"),
            "code_read_symbol by line should succeed: {result}"
        );
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["qualified_name"].as_str(), Some("add"));
    }

    #[tokio::test]
    async fn code_read_symbol_not_found() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::ReadSymbolParams {
            path: "src/math.rs".into(),
            symbol_or_line: "nonexistent_symbol_xyz".into(),
        });
        let result = server.code_read_symbol(params).await;
        let err: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(err["shape"], "Error");
        assert_eq!(err["code"], -32002, "should be NotFound: {result}");
        assert_eq!(err["kind"], "not_found");
        assert_eq!(err["data"]["symbol_or_line"], "nonexistent_symbol_xyz");
    }

    #[tokio::test]
    async fn code_read_symbol_has_parent_chain() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::ReadSymbolParams {
            path: "src/utils.rs".into(),
            symbol_or_line: "sum_three".into(),
        });
        let result = server.code_read_symbol(params).await;
        assert!(
            !result.starts_with("Error:"),
            "code_read_symbol should succeed: {result}"
        );
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["qualified_name"].as_str(), Some("sum_three"));
        // parent_chain may be empty for top-level symbols; just verify the field exists
        assert!(json.get("parent_chain").is_some());
    }

    #[tokio::test]
    async fn code_find_symbol_exact() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::FindSymbolParams {
            name: "add".into(),
            kind: None,
            lang: None,
        });
        let result = server.code_find_symbol(params).await;
        assert!(
            !result.starts_with("Error:"),
            "code_find_symbol should succeed: {result}"
        );
        let entries: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert!(!entries.is_empty(), "should find 'add' symbol: {result}");
        assert!(
            entries
                .iter()
                .any(|e| e["qualified_name"].as_str() == Some("add")),
            "should have 'add' in results"
        );
    }

    #[tokio::test]
    async fn code_find_symbol_with_kind_filter() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::FindSymbolParams {
            name: "add".into(),
            kind: Some("fn".into()),
            lang: None,
        });
        let result = server.code_find_symbol(params).await;
        assert!(
            !result.starts_with("Error:"),
            "code_find_symbol with kind filter should succeed: {result}"
        );
        let entries: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert!(!entries.is_empty(), "should find 'add' as a function");
    }

    #[tokio::test]
    async fn code_find_refs_has_results() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::FindRefsParams {
            symbol: "add".into(),
        });
        let result = server.code_find_refs(params).await;
        assert!(
            !result.starts_with("Error:"),
            "code_find_refs should succeed: {result}"
        );
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("ReferenceDigest"));
        assert_eq!(json["symbol"].as_str(), Some("add"));
        // There should be at least some refs (utils.rs uses add)
        assert!(json.get("total").is_some());
        assert!(json.get("top_k").is_some());
    }

    #[tokio::test]
    async fn code_search_exact_mode() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::SearchParams {
            query: "fn add".into(),
            mode: "exact".into(),
            filter: None,
        });
        let result = server.code_search(params).await;
        assert!(
            !result.starts_with("Error:"),
            "code_search exact should succeed: {result}"
        );
    }

    #[tokio::test]
    async fn code_search_regex_mode() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::SearchParams {
            query: r"fn\s+\w+\(a:\s*i32".into(),
            mode: "regex".into(),
            filter: None,
        });
        let result = server.code_search(params).await;
        assert!(
            !result.starts_with("Error:"),
            "code_search regex should succeed: {result}"
        );
        let entries: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert!(!entries.is_empty(), "regex search should find matches");
    }

    #[tokio::test]
    async fn code_search_with_git_modified_filter() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        // Filter with git_modified_only — on a non-git repo this should gracefully
        // fall back to returning all paths
        let params = Parameters(crate::tools::SearchParams {
            query: "fn".into(),
            mode: "exact".into(),
            filter: Some("git_modified:true".into()),
        });
        let result = server.code_search(params).await;
        // Should not crash even without git
        assert!(!result.starts_with("Error:"));
    }

    #[tokio::test]
    async fn code_skeleton_indexed_file() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::SkeletonParams {
            path: "src/math.rs".into(),
            depth: 1,
        });
        let result = server.code_skeleton(params).await;
        assert!(
            !result.starts_with("Error:"),
            "code_skeleton should succeed: {result}"
        );
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("Skeleton"));
        let content = json["content"].as_str().unwrap();
        assert!(content.contains("add"), "skeleton should contain 'add'");
        assert!(
            content.contains("{ ... }"),
            "skeleton should have elided bodies"
        );
    }

    #[tokio::test]
    async fn code_list_returns_files() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::ListParams {
            lang: Some("rust".into()),
            filter: None,
            glob: None,
        });
        let result = server.code_list(params).await;
        assert!(
            !result.starts_with("Error:"),
            "code_list should succeed: {result}"
        );
        let entries: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert!(
            entries.len() >= 2,
            "should list at least 2 Rust files, got {}",
            entries.len()
        );
    }

    #[tokio::test]
    async fn code_stats_after_indexing() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let result = server
            .code_stats(Parameters(crate::tools::StatsParams {}))
            .await;
        assert!(
            !result.starts_with("Error:"),
            "code_stats should succeed: {result}"
        );
        let stats: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(
            stats["files_indexed"].as_u64().unwrap_or(0) >= 2,
            "should have indexed at least 2 files"
        );
        assert!(
            stats["total_symbols"].as_u64().unwrap_or(0) > 0,
            "should have indexed symbols"
        );
    }

    #[tokio::test]
    async fn code_reindex_force() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;

        // Get stats before reindex
        let before = server
            .code_stats(Parameters(crate::tools::StatsParams {}))
            .await;
        let before_stats: serde_json::Value = serde_json::from_str(&before).unwrap();
        let before_files = before_stats["files_indexed"].as_u64().unwrap_or(0);

        // Force reindex
        let result = server
            .code_reindex(Parameters(crate::tools::ReindexParams { force: true }))
            .await;
        assert!(
            !result.starts_with("Error:"),
            "code_reindex force should succeed: {result}"
        );
        let reindex_stats: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(reindex_stats["status"].as_str(), Some("ok"));
        assert_eq!(reindex_stats["force"].as_bool(), Some(true));
        assert!(
            reindex_stats["files_indexed"].as_u64().unwrap_or(0) > 0,
            "force reindex should index files"
        );

        // Verify stats after reindex
        let after = server
            .code_stats(Parameters(crate::tools::StatsParams {}))
            .await;
        let after_stats: serde_json::Value = serde_json::from_str(&after).unwrap();
        let after_files = after_stats["files_indexed"].as_u64().unwrap_or(0);
        assert!(
            after_files >= before_files,
            "files after reindex ({after_files}) should be >= before ({before_files})"
        );
    }

    #[tokio::test]
    async fn code_multi_find_returns_results() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::MultiFindParams {
            patterns: vec!["fn add".into(), "fn mul".into()],
            filter: None,
        });
        let result = server.code_multi_find(params).await;
        assert!(
            !result.starts_with("Error:"),
            "code_multi_find should succeed: {result}"
        );
        let entries: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert!(
            !entries.is_empty(),
            "multi_find should return at least 1 pattern result"
        );
    }

    #[tokio::test]
    async fn code_find_strings_returns_results() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::FindStringsParams {
            pattern: "two".into(),
            kind: "comment".into(),
            filter: None,
        });
        let result = server.code_find_strings(params).await;
        assert!(
            !result.starts_with("Error:"),
            "code_find_strings should succeed: {result}"
        );
    }

    #[tokio::test]
    async fn query_tool_dispatch() {
        let (server, _tmp) = make_indexed_server().await;

        // Successful dispatch — should NOT be an error shape.
        let result = server.query_tool("code_stats", "{}").await;
        assert!(
            !result.contains(r#""shape":"Error""#),
            "query_tool should dispatch code_stats successfully: {result}"
        );

        // Unknown tool — structured NotFound (-32002).
        let result = server.query_tool("unknown_tool", "{}").await;
        let err: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(err["shape"], "Error");
        assert_eq!(err["code"], -32002);
        assert_eq!(err["kind"], "not_found");
        assert!(
            err["message"].as_str().unwrap().contains("unknown tool"),
            "unknown-tool message: {result}"
        );
        assert!(
            err["request_id"].as_str().unwrap().len() >= 20,
            "request_id must be a real ULID, got {result}"
        );

        // Invalid JSON args — structured InvalidParams (-32001).
        let result = server.query_tool("code_outline", "{invalid json").await;
        let err: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(err["shape"], "Error");
        assert_eq!(err["code"], -32001);
        assert_eq!(err["kind"], "invalid_params");
    }

    #[tokio::test]
    async fn query_tool_structured_errors_carry_request_id() {
        let (server, _tmp) = make_indexed_server().await;

        // Two back-to-back calls must produce distinct ULIDs — correlation is
        // the whole point of the request_id field.
        let a = server.query_tool("unknown_tool", "{}").await;
        let b = server.query_tool("unknown_tool", "{}").await;
        let a: serde_json::Value = serde_json::from_str(&a).unwrap();
        let b: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_ne!(
            a["request_id"], b["request_id"],
            "each query_tool call must get its own ULID"
        );
    }

    #[test]
    fn tool_error_produces_valid_structured_shape() {
        // Directly verify the Timeout error shape — the wrapper that fires it
        // lives inside tokio::time::timeout and is exercised in production
        // paths; this checks the wire contract it produces.
        let out = super::tool_error(
            ReconErrorCode::Timeout,
            "deadline exceeded",
            Some(serde_json::json!({ "tool": "foo", "timeout_secs": 30 })),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["shape"], "Error");
        assert_eq!(v["code"], -32003);
        assert_eq!(v["kind"], "timeout");
        assert_eq!(v["message"], "deadline exceeded");
        assert_eq!(v["data"]["tool"], "foo");
        assert_eq!(v["data"]["timeout_secs"], 30);
        assert!(v["request_id"].is_string());
    }

    #[tokio::test]
    async fn query_tool_returns_structured_not_found_on_missing_file() {
        let (server, _tmp) = make_indexed_server().await;
        let result = server
            .query_tool("code_outline", r#"{"path":"does/not/exist.rs"}"#)
            .await;
        let err: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(err["shape"], "Error");
        assert_eq!(err["code"], -32002); // NotFound
        assert_eq!(err["data"]["path"], "does/not/exist.rs");
    }
}
