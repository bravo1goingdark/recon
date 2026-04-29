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
use std::path::{Path, PathBuf};
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
    /// Cached call graph (forward + reverse CSR over `cached_symbols` ×
    /// `cached_refs`). Built lazily on first graph-tool call after each
    /// reindex; invalidated alongside `cached_symbols` / `cached_refs`.
    cached_call_graph: Arc<arc_swap::ArcSwapOption<recon_search::graph::CallGraph>>,
    /// Token-savings telemetry. Lock-free hot path; persisted to the
    /// `meta` table every `FLUSH_THRESHOLD` calls + on shutdown so
    /// lifetime totals survive restarts.
    telemetry: Arc<crate::telemetry::Telemetry>,
    /// Embedding service — set once in `init_embed`, read on every
    /// embed-backed tool call. Holds a trait object so the same struct
    /// works whether the binary was built with the hosted client
    /// (default) or the local fastembed backend (`--features
    /// local-embed`). `None` until `init_embed` resolves credentials /
    /// loads the model.
    ///
    /// Stored under `RwLock` rather than `arc_swap::ArcSwapOption`
    /// because the latter does not accept `?Sized` trait objects in
    /// the version we ship. Reads happen on the slow path (semantic
    /// search, watcher catch-up) so RwLock contention is in noise vs
    /// the inference round-trip itself.
    embed_service: Arc<parking_lot::RwLock<Option<Arc<dyn recon_core::embed::EmbedService>>>>,
    /// Lock-free read pool for vector similarity search. Always linked
    /// (sqlite-vec only, no ONNX); the storage layer is identical for
    /// hosted vs local — only the *generator* (the embed service)
    /// changes.
    vec_read_pool: Arc<arc_swap::ArcSwapOption<recon_embed::VecReadPool>>,
    /// Write handle — taken by `start_watcher`, None afterwards.
    vec_writer: Arc<Mutex<Option<recon_embed::VectorStore>>>,
    /// Cooperative shutdown flag — watcher loop polls this between batches.
    shutdown_flag: Arc<AtomicBool>,
    /// Wake-up channel for "shut down now, don't wait for the next signal."
    ///
    /// The serve loops (stdio + HTTP) `select!` on this in addition to
    /// SIGINT/SIGTERM and (for stdio) the MCP transport closing. The
    /// periodic license-revalidation task fires it when the worker
    /// rejects the key — without this, a deleted account would leave
    /// `recon serve` running forever, refusing tool calls but holding
    /// open watchers, ports, and SQLite handles. See
    /// [`ReconServer::request_shutdown`] for the trigger and
    /// [`ReconServer::await_shutdown_request`] for the consumer.
    shutdown_notify: Arc<tokio::sync::Notify>,
    /// Handle to the spawned watcher task so `shutdown()` can await its exit.
    watcher_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Current license, atomically swappable by the periodic re-validation
    /// task. `None` means "not enforced" — used by tests and direct library
    /// callers that bypass `recon serve`. The stdio `Command::Serve` path
    /// always populates this via [`ReconServer::set_license`].
    license: Arc<ArcSwap<Option<crate::license::ValidatedLicense>>>,
    /// Cache of measured-baseline token counts keyed by absolute path,
    /// valued by `(mtime_secs, tokens)`. The mtime in the value is what
    /// makes the cache self-invalidating: every lookup compares the
    /// current file mtime against the stored one; a mismatch is treated
    /// as a miss and the slot is overwritten. No explicit watcher hook
    /// is needed.
    ///
    /// Bounded at [`MAX_BASELINE_CACHE_ENTRIES`] to keep memory flat on
    /// long-running servers; on overflow we drop ~25 % of entries to
    /// retain warm files.
    measured_baseline_cache: Arc<dashmap::DashMap<PathBuf, (i64, u64)>>,
}

/// Bound on [`ReconServer::measured_baseline_cache`]. Sized to match
/// FffBackend's `MAX_CACHE_ENTRIES` so the two file-keyed caches scale
/// together — a hot file in one is overwhelmingly likely to be hot in
/// the other.
const MAX_BASELINE_CACHE_ENTRIES: usize = 2048;

fn redact_response(response: String) -> String {
    redact::redact_secrets(&response).unwrap_or(response)
}

/// Wrap a row-oriented tool response in the canonical [`ToolOutput::Hits`]
/// envelope and run secret redaction once on the final wire JSON.
///
/// `kind` selects the row schema (`"symbol"`, `"text"`, `"file"`,
/// `"string"`, `"multi_find"`, `"repo"`, `"savings"`); `cap` is the
/// tool-specific row limit — when `entries.len() >= cap`, the response
/// carries `truncated: true` so callers know results were capped.
///
/// The `entries` Vec is moved into `HitsView` (no clone) and serialised
/// once; net wire-size overhead vs the bare-array format is the
/// `{"shape":"Hits","kind":"…","count":N}` envelope (≈40 bytes) — well
/// under 1% of typical row-oriented responses.
fn hits_response(kind: &'static str, entries: Vec<serde_json::Value>, cap: usize) -> String {
    let truncated = entries.len() >= cap;
    let view = ToolOutput::Hits(HitsView {
        kind: kind.into(),
        count: entries.len(),
        hits: entries,
        truncated,
    });
    redact_response(serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")))
}

/// Build a search-hit JSON object, omitting `col` when not captured.
///
/// Token diet (v0.2.2): `"col":null` was emitted on every lexical hit and on
/// every Tantivy fallback hit even when no column was carried — pure overhead
/// for the LLM client. This helper lifts the conditional insertion into one
/// place so every search-tool site benefits identically.
fn text_hit_json(
    path: impl Into<String>,
    line: u32,
    col: Option<u32>,
    text: impl Into<String>,
) -> serde_json::Value {
    let mut map = serde_json::Map::with_capacity(4);
    map.insert("path".into(), serde_json::Value::String(path.into()));
    map.insert("line".into(), serde_json::Value::Number(line.into()));
    if let Some(c) = col {
        map.insert("col".into(), serde_json::Value::Number(c.into()));
    }
    map.insert("text".into(), serde_json::Value::String(text.into()));
    serde_json::Value::Object(map)
}

/// Maximum file size for `read_to_string` calls (2 MB).
/// Prevents OOM on accidentally large files (e.g. minified bundles, lock files).
const MAX_READ_FILE_SIZE: u64 = 2 * 1024 * 1024;

/// Cross-platform "is this the same file?" oracle.
///
/// Returns a stable identifier for the file at `path` that's only
/// meaningful for equality comparison: callers ask "did the file at
/// this path get replaced under me?" — they don't read the bits.
///
/// Implementation: delegates to the `file-id` crate, which wraps
/// `stat().st_ino` on Unix and `GetFileInformationByHandle` (returning
/// the NTFS file index) on Windows.  Doing this in std would require
/// the unstable `windows_by_handle` feature (rust-lang/rust#63010), so
/// the helper crate is the only stable path that works on both.
///
/// Returns `None` when the file is missing, inaccessible, or the
/// platform doesn't expose a file id (wasi, redox, …).  Callers should
/// treat `None` from a previously-`Some` reading as "the file is gone"
/// and handle it equivalently to "the file id changed."
fn file_id(path: &std::path::Path) -> Option<file_id::FileId> {
    file_id::get_file_id(path).ok()
}

/// Coalesces concurrent refresh requests into a single worker thread.
/// `dirty` is the edge: kick sets it; the worker drains it.
struct RefreshGate {
    in_flight: AtomicBool,
    dirty: AtomicBool,
}

static REFRESH_GATE: RefreshGate = RefreshGate {
    in_flight: AtomicBool::new(false),
    dirty: AtomicBool::new(false),
};

/// Spawn (or coalesce into) a background snapshot refresh.
///
/// On the watcher hot path we used to clear all caches synchronously. The
/// next read tool then paid `~350 ms` of cold `all_symbols + all_refs`. This
/// kicks the same refresh on a worker thread instead — reads keep serving
/// the previous (briefly stale) snapshot until the new one atomically lands.
///
/// Multiple kicks during an in-flight refresh collapse to one extra run, so
/// rapid save bursts produce at most two refreshes total.
fn kick_async_refresh(
    read_pool: &Arc<ReadPool>,
    cached_paths: &Arc<ArcSwap<Vec<PathBuf>>>,
    cached_file_count: &Arc<AtomicU64>,
    cached_symbols: &Arc<ArcSwap<Vec<recon_core::symbol::Symbol>>>,
    cached_refs: &Arc<ArcSwap<Vec<recon_core::symbol::Ref>>>,
    cached_call_graph: &Arc<arc_swap::ArcSwapOption<recon_search::graph::CallGraph>>,
) {
    REFRESH_GATE.dirty.store(true, Ordering::Release);
    if REFRESH_GATE
        .in_flight
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // Another worker is already running; it will see the `dirty` flag
        // and re-run after its current snapshot lands.
        return;
    }
    let read_pool = read_pool.clone();
    let cached_paths = cached_paths.clone();
    let cached_file_count = cached_file_count.clone();
    let cached_symbols = cached_symbols.clone();
    let cached_refs = cached_refs.clone();
    let cached_call_graph = cached_call_graph.clone();
    std::thread::spawn(move || {
        loop {
            // Edge-triggered: clear `dirty` before the snapshot so a kick
            // that arrives mid-snapshot retriggers another iteration.
            REFRESH_GATE.dirty.store(false, Ordering::Release);
            match read_pool.snapshot_all_for_caches() {
                Ok((paths, symbols, refs)) => {
                    cached_file_count.store(paths.len() as u64, Ordering::Relaxed);
                    cached_paths.store(Arc::new(paths));
                    cached_symbols.store(Arc::new(symbols));
                    cached_refs.store(Arc::new(refs));
                    cached_call_graph.store(None);
                }
                Err(e) => warn!("async cache refresh failed: {e}"),
            }
            // Release first, then recheck `dirty`. If a kick arrives in this
            // gap and claims `in_flight` before us, we lose the re-claim and
            // exit — its thread will pick up the work.
            REFRESH_GATE.in_flight.store(false, Ordering::Release);
            if !REFRESH_GATE.dirty.load(Ordering::Acquire) {
                break;
            }
            if REFRESH_GATE
                .in_flight
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                break;
            }
        }
    });
}

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
        // Canonicalize once at construction so `resolve_path`'s
        // `canonical.starts_with(&self.repo_root)` check works on platforms
        // where the input path differs from its canonical form (notably
        // macOS `/var` → `/private/var`, symlinked parent directories).
        // Fall back to the raw path if the root doesn't exist yet —
        // construction-time failure would regress behavior for callers
        // that create the root lazily.
        let repo_root = std::fs::canonicalize(&repo_root).unwrap_or(repo_root);

        let writer = match tantivy.writer(50_000_000) {
            Ok(w) => Some(w),
            Err(e) => {
                warn!(
                    %e,
                    "tantivy writer creation failed at startup; \
                     BM25 search will be degraded until restart \
                     (most often a stale lock from a previously killed process — \
                     check the index dir for a leftover .tantivy-writer.lock)"
                );
                None
            }
        };
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
        // Hydrate telemetry counters from the meta table BEFORE the
        // store is moved into the Mutex. Best-effort: a corrupt DB
        // resets counters to zero; never blocks startup.
        let telemetry = Arc::new(crate::telemetry::Telemetry::new());
        telemetry.hydrate_from_store(&store);

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
            cached_call_graph: Arc::new(arc_swap::ArcSwapOption::const_empty()),
            telemetry,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            watcher_handle: Arc::new(Mutex::new(None)),
            license: Arc::new(ArcSwap::new(Arc::new(None))),
            measured_baseline_cache: Arc::new(dashmap::DashMap::with_capacity(
                MAX_BASELINE_CACHE_ENTRIES,
            )),
            embed_service: Arc::new(parking_lot::RwLock::new(None)),
            vec_read_pool: Arc::new(arc_swap::ArcSwapOption::const_empty()),
            vec_writer: Arc::new(Mutex::new(None)),
        })
    }

    /// Refresh all cached data from the database.
    /// Called after initial index and reindex to keep caches warm.
    fn refresh_caches(&self) {
        // Single transactional snapshot — paths, symbols, and refs all reflect
        // the same SQLite state. Three separate reads would let a concurrent
        // writer interleave and produce mutually inconsistent caches.
        match self.read_pool.snapshot_all_for_caches() {
            Ok((paths, symbols, refs)) => {
                self.cached_file_count
                    .store(paths.len() as u64, Ordering::Relaxed);
                self.cached_paths.store(Arc::new(paths));
                self.cached_symbols.store(Arc::new(symbols));
                self.cached_refs.store(Arc::new(refs));
                // Graph derives from symbols+refs; invalidate so next access rebuilds.
                self.cached_call_graph.store(None);
            }
            Err(e) => warn!("failed to refresh caches: {e}"),
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

    /// Get the cached call graph, building it lazily from cached_symbols ×
    /// cached_refs on first access after each cache invalidation.
    fn cached_call_graph(&self) -> Arc<recon_search::graph::CallGraph> {
        if let Some(g) = self.cached_call_graph.load_full() {
            return g;
        }
        let symbols = self.cached_all_symbols();
        let refs = self.cached_all_refs();
        let graph = Arc::new(recon_search::graph::CallGraph::build(&symbols, &refs));
        self.cached_call_graph.store(Some(graph.clone()));
        graph
    }

    /// Resolve a name to symbol indices in `symbols`. Resolution policy
    /// (most specific first; case-sensitive before case-insensitive at
    /// every tier so `Handler` doesn't ambiguously match a `handler`
    /// module): exact qname → fuzzy qname → exact name → fuzzy name.
    fn resolve_symbol_to_indices(symbols: &[recon_core::symbol::Symbol], name: &str) -> Vec<u32> {
        let by = |pred: &dyn Fn(&recon_core::symbol::Symbol) -> bool| -> Vec<u32> {
            symbols
                .iter()
                .enumerate()
                .filter(|(_, s)| pred(s))
                .map(|(i, _)| i as u32)
                .collect()
        };
        let hits = by(&|s| s.qualified_name.as_str() == name);
        if !hits.is_empty() {
            return hits;
        }
        let hits = by(&|s| s.qualified_name.as_str().eq_ignore_ascii_case(name));
        if !hits.is_empty() {
            return hits;
        }
        let hits = by(&|s| s.name.as_str() == name);
        if !hits.is_empty() {
            return hits;
        }
        by(&|s| s.name.as_str().eq_ignore_ascii_case(name))
    }

    /// Map a symbol index into a `SymbolHop` for graph responses.
    fn symbol_hop_for_idx(
        symbols: &[recon_core::symbol::Symbol],
        idx: u32,
    ) -> recon_core::shapes::SymbolHop {
        let s = &symbols[idx as usize];
        recon_core::shapes::SymbolHop {
            qualified_name: s.qualified_name.to_string(),
            kind: s.kind,
            path: (*s.path).clone(),
            line: *s.line_range.start(),
        }
    }

    /// Walk a symbol's `parent_id` chain back to a top-level ancestor and
    /// return the qualified-name chain outermost-first.
    fn parent_chain_for(symbols: &[recon_core::symbol::Symbol], idx: u32) -> Vec<String> {
        let mut chain: Vec<String> = Vec::new();
        let mut cur = symbols[idx as usize].parent_id;
        let mut guard: usize = 0;
        while let Some(pid) = cur {
            if pid == 0 {
                break;
            }
            let parent_idx = symbols.iter().position(|s| s.id == pid);
            match parent_idx {
                Some(i) => {
                    chain.push(symbols[i].qualified_name.to_string());
                    cur = symbols[i].parent_id;
                }
                None => break,
            }
            guard += 1;
            if guard > 32 {
                break;
            }
        }
        chain.reverse();
        chain
    }

    /// Best-effort test-symbol detector. Rust + generic test_*/Test* names.
    fn is_phase1_test_symbol(sym: &recon_core::symbol::Symbol) -> bool {
        let q = sym.qualified_name.as_str();
        if q == "tests"
            || q.starts_with("tests::")
            || q.contains("::tests::")
            || q.ends_with("::tests")
        {
            return true;
        }
        let name = sym.name.as_str();
        name.starts_with("test_") || name.starts_with("Test")
    }

    /// Read-only access to the in-memory telemetry counters. Exposed
    /// for the calibration xtask and any external integration test
    /// that needs to introspect per-tool measured / static splits
    /// without going through the SQLite-backed `code_savings` tool.
    pub fn telemetry_arc(&self) -> Arc<crate::telemetry::Telemetry> {
        self.telemetry.clone()
    }

    /// Record one tool call into telemetry. Lock-free hot path; if a
    /// flush threshold is reached, schedule an async write.
    ///
    /// `measured_baseline` is `Some(n)` when the handler ran with
    /// `RECON_MEASURED_BASELINES=1` and computed a real Read/grep
    /// alternative number for this call. The static [`BASELINES`]
    /// credit is added regardless via `Telemetry::record`.
    fn record_call(
        &self,
        tool: &'static str,
        started_at: std::time::Instant,
        response: &str,
        measured_baseline: Option<u64>,
    ) {
        let response_tokens = recon_search::tokens::estimate_tokens(response) as u64;
        let should_flush = self.telemetry.record(
            tool,
            started_at.elapsed(),
            response_tokens,
            measured_baseline,
        );
        if should_flush {
            self.flush_telemetry_async();
        }
    }

    /// Higher-order wrapper that times a tool's execution and records it.
    /// Each `code_*` handler wraps its body in `self.instrumented(...)`.
    /// Used by tools that don't (or can't) supply a measured baseline —
    /// the static [`BASELINES`] entry is the only signal.
    async fn instrumented<Fut>(&self, tool: &'static str, fut: Fut) -> String
    where
        Fut: std::future::Future<Output = String>,
    {
        let started_at = std::time::Instant::now();
        let result = fut.await;
        self.record_call(tool, started_at, &result, None);
        result
    }

    /// Variant of [`Self::instrumented`] for handlers that can supply a
    /// per-call measured baseline (the 9 bucket-1 tools). The future
    /// resolves to `(response, measured_baseline)`; when
    /// `measure_baselines` is off the handler should pass `None` and
    /// behave identically to the non-measured wrapper. Centralising
    /// the convention here keeps the call site shape uniform across
    /// handlers and makes the flag-off code path trivially auditable.
    async fn instrumented_measured<Fut>(&self, tool: &'static str, fut: Fut) -> String
    where
        Fut: std::future::Future<Output = (String, Option<u64>)>,
    {
        let started_at = std::time::Instant::now();
        let (result, measured) = fut.await;
        self.record_call(tool, started_at, &result, measured);
        result
    }

    /// Compute the "what would Read of this file have cost" baseline,
    /// in tokens. Reuses the same `MAX_READ_FILE_SIZE` cap that real
    /// Read-shaped handlers apply (see `code_skeleton` at line ~1828)
    /// so the baseline reflects what the agent would actually have
    /// been able to read.
    ///
    /// Returns `None` when the file is too large or unreadable —
    /// those are cases where reporting a number would be misleading.
    /// The caller passes this straight through to
    /// [`Self::instrumented_measured`].
    ///
    /// Cached via [`ReconServer::measured_baseline_cache`] keyed by
    /// `(path, mtime_secs)`. A typical session calls `code_outline`
    /// then `code_read_symbol` (and often `code_context`) on the
    /// same file — without the cache that's three full reads of
    /// identical bytes just to recompute the baseline. The cache is
    /// self-invalidating: an mtime mismatch is treated as a miss and
    /// the slot is overwritten.
    async fn measure_read_baseline(&self, abs_path: &Path) -> Option<u64> {
        let meta = tokio::fs::metadata(abs_path).await.ok()?;
        if meta.len() > MAX_READ_FILE_SIZE {
            return None;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        if let Some(entry) = self.measured_baseline_cache.get(abs_path) {
            let (cached_mtime, cached_tokens) = *entry;
            if cached_mtime == mtime {
                return Some(cached_tokens);
            }
        }

        let content = tokio::fs::read_to_string(abs_path).await.ok()?;
        let tokens = recon_search::tokens::estimate_tokens(&content) as u64;

        // Bound growth: drop ~25 % on overflow to retain warm entries.
        if self.measured_baseline_cache.len() >= MAX_BASELINE_CACHE_ENTRIES {
            let to_remove: Vec<PathBuf> = self
                .measured_baseline_cache
                .iter()
                .take(MAX_BASELINE_CACHE_ENTRIES / 4)
                .map(|e| e.key().clone())
                .collect();
            for key in to_remove {
                self.measured_baseline_cache.remove(&key);
            }
        }
        self.measured_baseline_cache
            .insert(abs_path.to_path_buf(), (mtime, tokens));
        Some(tokens)
    }

    /// Spawn an async task to persist lifetime telemetry. Hot-path
    /// callers must not block on this.
    fn flush_telemetry_async(&self) {
        let telemetry = self.telemetry.clone();
        let store = self.write_store.clone();
        tokio::task::spawn_blocking(move || {
            let guard = store.lock();
            telemetry.flush_to_store(&guard);
        });
    }

    /// Spawn a periodic telemetry-flush task so even idle sessions
    /// persist counters at least once per
    /// [`crate::telemetry::FLUSH_INTERVAL_SECS`]. The count-based
    /// threshold ([`crate::telemetry::FLUSH_THRESHOLD`]) still fires on
    /// hot bursts; the timer covers the long tail (3 calls/hr in an
    /// otherwise-idle IDE window).
    ///
    /// Override the interval via `RECON_TELEMETRY_FLUSH_SECS`. Setting
    /// it to `0` disables the timer entirely (the count trigger keeps
    /// working). Clamped to [10, 3600] otherwise.
    ///
    /// The task holds a clone of the server (cheap — all state is
    /// behind `Arc`) and exits when `shutdown_flag` is set, so it
    /// terminates cleanly with the rest of `recon serve`.
    pub fn start_telemetry_flush_timer(&self) {
        let interval_secs = match std::env::var("RECON_TELEMETRY_FLUSH_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
        {
            Some(0) => {
                info!("telemetry: periodic flush disabled (RECON_TELEMETRY_FLUSH_SECS=0)");
                return;
            }
            Some(n) => n.clamp(10, 3600),
            None => crate::telemetry::FLUSH_INTERVAL_SECS,
        };
        let server = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the immediate-fire so we wait one full interval before
            // the first flush — fresh counters have nothing to persist.
            tick.tick().await;
            loop {
                tick.tick().await;
                if server
                    .shutdown_flag
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    break;
                }
                server.flush_telemetry_async();
            }
        });
    }

    /// Synchronous flush — used by `shutdown()` to capture the trailing
    /// window before exit.
    fn flush_telemetry_sync(&self) {
        let store = self.write_store.lock();
        self.telemetry.flush_to_store(&store);
    }

    /// Shared body for `code_callers` (`reverse=true`) and
    /// `code_callees` (`reverse=false`). Layered BFS over the cached
    /// call graph, capped per [`recon_search::graph::GraphCaps`].
    async fn callers_or_callees_inner(
        &self,
        params: Parameters<CallersParams>,
        reverse: bool,
    ) -> String {
        let depth = params.0.depth;
        if depth == 0 {
            return tool_error(ReconErrorCode::InvalidParams, "depth must be >= 1", None);
        }
        let depth = depth.min(recon_search::graph::MAX_ALLOWED_DEPTH);
        let symbols = self.cached_all_symbols();
        let seeds = Self::resolve_symbol_to_indices(&symbols, &params.0.symbol);
        if seeds.is_empty() {
            return tool_error(
                ReconErrorCode::NotFound,
                format!("symbol not found: {}", params.0.symbol),
                Some(serde_json::json!({ "symbol": params.0.symbol })),
            );
        }
        let graph = self.cached_call_graph();
        let caps = recon_search::graph::GraphCaps::default_for_callers(depth);
        let result = if reverse {
            graph.transitive_callers(&seeds, &caps)
        } else {
            graph.transitive_callees(&seeds, &caps)
        };
        let total: usize = result.tiers.iter().map(|t| t.nodes.len()).sum();
        let tiers: Vec<recon_core::shapes::RefTier> = result
            .tiers
            .iter()
            .map(|t| recon_core::shapes::RefTier {
                depth: t.depth,
                refs: t
                    .nodes
                    .iter()
                    .map(|&i| Self::symbol_hop_for_idx(&symbols, i))
                    .collect(),
                truncated: t.truncated_at_cap,
            })
            .collect();
        let view = ToolOutput::ReferenceDigest(RefDigestView {
            symbol: params.0.symbol.as_str().into(),
            total,
            top_k: vec![],
            path: vec![],
            tiers,
            truncated: result.truncated,
            unresolved_hint: None,
            tests: vec![],
        });
        redact_response(serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")))
    }

    /// Initialize the embedding engine + open the vector store.
    ///
    /// Two backends, switched at compile time:
    ///
    /// - **Default** (no extra features): hosted client via
    ///   [`recon_embed_client::HostedEmbedService`]. Reads the API key
    ///   from the credentials file (`recon login` must have run);
    ///   returns successfully with no embed service if credentials are
    ///   missing or `RECON_NO_EMBED=1` is set, so `recon serve` still
    ///   starts and lexical-only tools keep working.
    /// - **`--features local-embed`**: spawns a fastembed/ONNX worker
    ///   thread and stores the resulting `recon_embed::EmbedService`
    ///   adapted to the trait.
    ///
    /// Vector storage opens unconditionally — the storage layer is
    /// identical whether the generator is local or hosted.
    pub async fn init_embed(&self) -> Result<(), recon_core::error::Error> {
        let vec_dir = self.repo_root.join(".recon").join("vectors");

        let svc = self.build_embed_service()?;
        let vs = recon_embed::VectorStore::open(&vec_dir)
            .map_err(|e| recon_core::error::Error::Search(format!("vector store open: {e}")))?;
        let pool = Arc::new(
            recon_embed::VecReadPool::new(&vec_dir, 4)
                .map_err(|e| recon_core::error::Error::Search(format!("vec read pool: {e}")))?,
        );
        if let Some(svc) = svc {
            *self.embed_service.write() = Some(svc);
            info!("embed service initialized");
        } else {
            warn!(
                "embed service unavailable (no credentials or RECON_NO_EMBED=1) — \
                 semantic search will fail closed; lexical search is unaffected"
            );
        }
        self.vec_read_pool.store(Some(pool));
        *self.vec_writer.lock() = Some(vs);
        Ok(())
    }

    /// Default backend: hosted client. Credentials missing → `None`,
    /// not an error: server still starts.
    #[cfg(not(feature = "local-embed"))]
    fn build_embed_service(
        &self,
    ) -> Result<Option<Arc<dyn recon_core::embed::EmbedService>>, recon_core::error::Error> {
        Ok(recon_embed_client::HostedEmbedService::from_env()
            .map(|s| Arc::new(s) as Arc<dyn recon_core::embed::EmbedService>))
    }

    /// Local fastembed backend. Reads from `RECON_EMBED_DIR` if set,
    /// otherwise downloads the default Jina v2-base-code model on
    /// first run.
    #[cfg(feature = "local-embed")]
    fn build_embed_service(
        &self,
    ) -> Result<Option<Arc<dyn recon_core::embed::EmbedService>>, recon_core::error::Error> {
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
        Ok(Some(svc as Arc<dyn recon_core::embed::EmbedService>))
    }

    /// Run initial indexing of the repo (SQLite + Tantivy).
    pub async fn index_repo(&self) -> Result<(), recon_core::error::Error> {
        // Phase A: index. Both writers locked together — `index_repo_incremental`
        // writes to SQLite then commits Tantivy.
        let stats = {
            let store = self.write_store.lock();
            let mut tw = self.tantivy_writer.lock();
            indexer::index_repo_incremental(
                &store,
                Some(&self.tantivy),
                &self.repo_root,
                tw.as_mut(),
            )?
        }; // Both locks released here.

        info!(
            files = stats.files_indexed,
            symbols = stats.total_symbols,
            "initial indexing complete"
        );

        // Phase B: VACUUM with only the SQLite writer lock held — Tantivy
        // is free for any concurrent reader. Runs once at startup, but
        // keeping the lock surface narrow makes the function reusable.
        {
            let store = self.write_store.lock();
            store.incremental_vacuum().map_err(|e| {
                recon_core::error::Error::Storage(format!("incremental_vacuum: {e}"))
            })?;
        }

        // Phase C: pre-warm caches (no locks held).
        self.refresh_caches();

        // Phase D: pre-warm the repo_map cache via a short write-lock for the meta upsert.
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

    /// Run a Tantivy search on a blocking thread pool.
    ///
    /// `TantivyBackend::search` is `&self` and lock-free internally, but
    /// the body is CPU-bound (query parser + top-k collector) and can take
    /// 5–20 ms on a 500K-LOC index. CLAUDE.md: *"tantivy calls always need
    /// [spawn_blocking]"* — without it, one search stalls every other
    /// tokio task co-scheduled on the same worker thread.
    ///
    /// Errors from Tantivy are swallowed into an empty result (same UX as
    /// the previous inline `.unwrap_or_default()` chain) because a failed
    /// BM25 index pass is a tier fallback, not a user-visible error.
    async fn tantivy_search(
        &self,
        query: String,
        limit: usize,
    ) -> Vec<recon_search::tantivy_backend::StructuredHit> {
        let tantivy = self.tantivy.clone();
        tokio::task::spawn_blocking(move || tantivy.search(&query, limit))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default()
    }

    /// Install / swap the active license.
    ///
    /// Used by `Command::Serve` at startup (after `validate_license_or_die`)
    /// and by the periodic re-validation task so the expiry gate in
    /// [`query_tool`] sees the current billing state atomically.
    pub fn set_license(&self, license: crate::license::ValidatedLicense) {
        self.license.store(Arc::new(Some(license)));
    }

    /// Return a snapshot of the current license, if one is installed.
    pub fn current_license(&self) -> Option<crate::license::ValidatedLicense> {
        self.license.load().as_ref().clone()
    }

    /// The repository root this server indexes. Borrowed (no clone).
    pub fn repo_root(&self) -> &std::path::Path {
        &self.repo_root
    }

    /// Cached file count (updated on every index/reindex). Lock-free read.
    pub fn file_count(&self) -> u64 {
        self.cached_file_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Cached symbol count (updated on every index/reindex). Lock-free read —
    /// reads the length of the cached symbols vector via `ArcSwap`.
    pub fn symbol_count(&self) -> u64 {
        self.cached_symbols.load_full().len() as u64
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

        // Pre-flight: if the cached license has expired, short-circuit with a
        // clear renewal message. `current_license() == None` means the server
        // is running in a library-test context and expiry enforcement is off —
        // `Command::Serve` always installs a license via `set_license`.
        if let Some(license) = self.current_license() {
            if license.is_expired() {
                return REQUEST_ID
                    .scope(request_id.clone(), async move {
                        tool_error(
                            ReconErrorCode::LicenseExpired,
                            format!(
                                "License expired on {}. Run `recon login <key>` to renew, \
                                 or resubscribe at https://mcprecon.pages.dev/dashboard",
                                license.expiry_string()
                            ),
                            Some(serde_json::json!({
                                "tier": license.tier.name(),
                                "expires_at": license.expires_at,
                            })),
                        )
                    })
                    .instrument(span)
                    .await;
            }
        }

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
            "code_path" => match serde_json::from_str::<PathParams>(args_json) {
                Ok(p) => self.code_path(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_callers" => match serde_json::from_str::<CallersParams>(args_json) {
                Ok(p) => self.code_callers(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_callees" => match serde_json::from_str::<CallersParams>(args_json) {
                Ok(p) => self.code_callees(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_context" => match serde_json::from_str::<ContextParams>(args_json) {
                Ok(p) => self.code_context(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_impact" => match serde_json::from_str::<ImpactParams>(args_json) {
                Ok(p) => self.code_impact(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_subsystems" => match serde_json::from_str::<SubsystemsParams>(args_json) {
                Ok(p) => self.code_subsystems(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_subsystem" => match serde_json::from_str::<SubsystemParams>(args_json) {
                Ok(p) => self.code_subsystem(Parameters(p)).await,
                Err(e) => tool_error_invalid_args(&e),
            },
            "code_savings" => match serde_json::from_str::<SavingsParams>(args_json) {
                Ok(p) => self.code_savings(Parameters(p)).await,
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
        let cached_call_graph = self.cached_call_graph.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let shutdown_notify = self.shutdown_notify.clone();
        // Capture the initial file-id of `.recon/index.db` so we can detect
        // it being unlinked / replaced from underneath us. This happens
        // when a misbehaving test or `rm -rf .recon/` clobbers the dir
        // while we're running. Without this guard, the OS keeps our open
        // file handles alive against a now-orphaned file while new tools/
        // CLI invocations write to a fresh file at the same path — silent
        // split-brain.
        //
        // Cross-platform via the `file-id` crate (see `file_id` helper):
        // Unix inode on Linux/macOS, NTFS file index on Windows. Modern
        // SQLite opens with FILE_SHARE_DELETE on Windows, so the
        // deleted-while-open case is reachable there too — the file is
        // marked for deletion and lingers until our last handle closes.
        // Other platforms (wasi, redox, …): the helper returns `None`,
        // the check below short-circuits, and we keep v0.3.3 behavior.
        let initial_db_inode: Option<file_id::FileId> = file_id(&repo_root.join(".recon/index.db"));
        // Snapshot the Arc handles once; the hot path inside the loop needs no locks.
        // Trait-object handle works for both the hosted client and the local
        // fastembed backend.
        let embed_svc: Option<Arc<dyn recon_core::embed::EmbedService>> =
            self.embed_service.read().clone();
        let vec_pool: Option<Arc<recon_embed::VecReadPool>> = self.vec_read_pool.load_full();
        // Take the write handle — watcher owns it exclusively from here.
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

            // Size pools to all worker cores so multi-file batches (rebase,
            // format-on-save sweep) parse in parallel; per-language pools are
            // cheap and the watcher thread sleeps between bursts.
            let pools = LanguagePools::new(rayon::current_num_threads().max(4));

            // ── One-time catch-up: embed any symbols not yet in the vector store ──
            // Runs before the event loop so the watcher thread owns vec_writer exclusively.
            // Skipped when `embed_svc` is None (no credentials, RECON_NO_EMBED=1, or
            // hosted endpoint unreachable at startup) — semantic search degrades to
            // no-op, lexical-only paths keep working.
            if let (Some(ref svc), Some(ref pool), Some(ref writer)) =
                (&embed_svc, &vec_pool, &vec_writer)
            {
                const EMBED_BATCH: usize = 64;
                match read_pool.all_symbols() {
                    Err(e) => warn!("embed catch-up: all_symbols: {e}"),
                    Ok(all_syms) => {
                        // Orphan cleanup: any embed_meta row whose symbol no
                        // longer exists in SQLite. Important after deletes that
                        // happened before the watcher delete-fix shipped, or
                        // when SQLite is wiped/restored out-of-band.
                        let symbol_id_set: ahash::AHashSet<u64> =
                            all_syms.iter().map(|s| s.id).collect();
                        match pool.all_embed_ids() {
                            Ok(embed_ids) => {
                                let to_delete: Vec<u64> = embed_ids
                                    .into_iter()
                                    .filter(|id| !symbol_id_set.contains(id))
                                    .collect();
                                if !to_delete.is_empty() {
                                    let count = to_delete.len();
                                    if let Err(e) = writer.delete_by_symbol_ids(&to_delete) {
                                        warn!("embed catch-up: orphan cleanup: {e}");
                                    } else {
                                        info!(
                                            orphans = count,
                                            "embed catch-up: removed orphan embeddings"
                                        );
                                    }
                                }
                            }
                            Err(e) => warn!("embed catch-up: all_embed_ids: {e}"),
                        }

                        if !all_syms.is_empty() {
                            let all_ids: Vec<u64> = all_syms.iter().map(|s| s.id).collect();
                            let existing = pool.existing_hashes(&all_ids).unwrap_or_else(|e| {
                                warn!("embed catch-up: existing_hashes: {e}");
                                AHashMap::new()
                            });
                            let to_embed: Vec<&recon_core::symbol::Symbol> = all_syms
                                .iter()
                                .filter(|s| existing.get(&s.id).is_none_or(|h| *h != s.body_hash))
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
                                            warn!(
                                                ?rel_path,
                                                "embed catch-up: cannot read file: {e}"
                                            );
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
                                                recon_embed::format_symbol(s, body)
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
                        } // closes `if !all_syms.is_empty()`
                    }
                }
            }

            loop {
                if shutdown_flag.load(Ordering::Relaxed) {
                    debug!("watcher: shutdown flag set, exiting loop");
                    break;
                }

                // Self-heal guard: if `.recon/index.db` was unlinked
                // or replaced since we started (rare; happens when a
                // sibling process `rm -rf .recon/`s us, a misbehaving
                // test wipes the dir, or a container restart races
                // with our shutdown), our open file handles now point
                // at an orphaned file. Continuing the loop means
                // writing into a phantom DB nothing else can see —
                // silent split-brain. Better to exit cleanly so the
                // IDE supervisor respawns us against the live file.
                //
                // Cross-platform via `file_id`: Unix inode, Windows
                // NTFS file-index, None elsewhere (no-op fallback).
                if let Some(ref initial) = initial_db_inode {
                    let current = file_id(&repo_root.join(".recon/index.db"));
                    if current.as_ref() != Some(initial) {
                        warn!(
                            initial_id = ?initial,
                            current_id = ?current,
                            "watcher: .recon/index.db file-id changed under us; \
                             requesting shutdown so the supervisor respawns against the live file",
                        );
                        shutdown_flag.store(true, Ordering::Relaxed);
                        shutdown_notify.notify_waiters();
                        break;
                    }
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
                    // Phase 0: Partition into existing-on-disk vs deleted paths.
                    // Filesystem state at this moment is the source of truth — an
                    // editor's atomic-rename save (write-tmp + delete + rename)
                    // collapses to "exists" by Phase 0 time, which is correct.
                    let mut existing_paths: Vec<PathBuf> = Vec::with_capacity(changed_paths.len());
                    let mut deleted_paths: Vec<PathBuf> = Vec::new();
                    for path in changed_paths {
                        if path.exists() {
                            existing_paths.push(path);
                        } else {
                            deleted_paths.push(path);
                        }
                    }

                    let mut did_delete = false;
                    if !deleted_paths.is_empty() {
                        // Snapshot symbol IDs BEFORE the SQLite cascade — embeddings
                        // live in a separate db and must be cleaned up by ID.
                        let mut deleted_symbol_ids: Vec<u64> = Vec::new();
                        if vec_writer.is_some() {
                            for abs_path in &deleted_paths {
                                let rel_path =
                                    abs_path.strip_prefix(&repo_root).unwrap_or(abs_path);
                                match read_pool.symbols_for_path(rel_path) {
                                    Ok(syms) => {
                                        deleted_symbol_ids.extend(syms.into_iter().map(|s| s.id))
                                    }
                                    Err(e) => {
                                        warn!(?rel_path, "watcher: symbols_for_path on delete: {e}")
                                    }
                                }
                            }
                        }

                        // SQLite cascade — drops file + symbols + refs in one transaction.
                        {
                            let rel_paths: Vec<&std::path::Path> = deleted_paths
                                .iter()
                                .map(|abs| abs.strip_prefix(&repo_root).unwrap_or(abs))
                                .collect();
                            debug!(count = rel_paths.len(), "watcher: cascading delete");
                            let store = write_store.lock();
                            match store.delete_files_cascade(&rel_paths) {
                                Ok(()) => did_delete = true,
                                Err(e) => warn!(
                                    count = rel_paths.len(),
                                    "watcher: delete_files_cascade: {e}"
                                ),
                            }
                        }

                        // Tantivy delete by path.
                        {
                            let mut tw = tantivy_writer.lock();
                            if let Some(ref mut writer) = *tw {
                                for abs_path in &deleted_paths {
                                    let rel_path =
                                        abs_path.strip_prefix(&repo_root).unwrap_or(abs_path);
                                    tantivy.delete_path(writer, rel_path);
                                }
                                if let Err(e) = tantivy.commit(writer) {
                                    warn!("watcher: tantivy commit (delete): {e}");
                                }
                            }
                        }

                        // Vector store delete — exclusive writer, no lock.
                        // Note: orphan embeddings from deletes that happened before
                        // this fix shipped are not cleaned up here; force-reindex
                        // remains the recovery path for those.
                        if let Some(ref writer) = vec_writer {
                            if !deleted_symbol_ids.is_empty() {
                                if let Err(e) = writer.delete_by_symbol_ids(&deleted_symbol_ids) {
                                    warn!("watcher: vector delete: {e}");
                                }
                            }
                        }
                    }

                    // Phase 1: Filter to files that actually changed (lock-free via ReadPool)
                    let to_parse: Vec<(PathBuf, Vec<u8>)> = existing_paths
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
                        // Even with no parse work, deletes invalidate caches.
                        // Kick async refresh; keep serving prev snapshot until it lands.
                        if did_delete {
                            kick_async_refresh(
                                &read_pool,
                                &cached_paths,
                                &cached_file_count,
                                &cached_symbols,
                                &cached_refs,
                                &cached_call_graph,
                            );
                        }
                        return;
                    }

                    // Phase 2: Parse all files in parallel (NO locks held —
                    // pure CPU work). `parse_file_with_content` is pure;
                    // `LanguagePools` is `Arc`-cloned internally per parser.
                    use rayon::prelude::*;
                    let parsed: Vec<indexer::ParsedFile> = to_parse
                        .par_iter()
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
                    // No-op when embed_svc is None (no credentials, RECON_NO_EMBED=1,
                    // or hosted endpoint unreachable at startup).
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
                                .filter(|s| existing.get(&s.id).is_none_or(|h| *h != s.body_hash))
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
                                            recon_embed::format_symbol(s, body)
                                        })
                                        .collect();

                                    // Channel send (local) or HTTP round-trip (hosted) —
                                    // no lock either way; the trait abstracts both.
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

                    // Refresh caches asynchronously. Reads continue serving the
                    // previous snapshot (briefly stale) until the new one lands —
                    // strictly better than the old behavior, which cleared the
                    // caches and forced the next read to pay ~350 ms of cold
                    // `all_symbols` + `all_refs` synchronously.
                    kick_async_refresh(
                        &read_pool,
                        &cached_paths,
                        &cached_file_count,
                        &cached_symbols,
                        &cached_refs,
                        &cached_call_graph,
                    );
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
    /// Request a clean shutdown of the running server from outside the
    /// serve loop — used when the periodic license re-validation task
    /// detects a Rejected response (account deletion, key revoke,
    /// subscription hard-expiry) or when the cached credentials vanish
    /// from disk (`recon logout` against a running session).
    ///
    /// Sets the cooperative shutdown flag (so the watcher loop bails
    /// at its next 500 ms poll) and notifies the serve-loop's
    /// `tokio::select!`. The serve loop then performs the same
    /// teardown as a SIGTERM: drains the watcher, commits Tantivy,
    /// flushes telemetry, vacuums SQLite. Idempotent — repeated calls
    /// are noops after the first.
    pub fn request_shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        self.shutdown_notify.notify_waiters();
    }

    /// Wait until [`request_shutdown`] is called. The serve loops
    /// (stdio + HTTP) `select!` on this alongside their existing signal
    /// and transport-close waiters. Returns immediately if a shutdown
    /// was already requested before the await.
    pub async fn await_shutdown_request(&self) {
        // Fast path: already requested. notified() on a Notify with no
        // permits would block forever in this case, so check the flag
        // first and short-circuit.
        if self.shutdown_flag.load(Ordering::Relaxed) {
            return;
        }
        self.shutdown_notify.notified().await;
    }

    /// Final teardown — drain the watcher, commit Tantivy, flush
    /// telemetry, vacuum SQLite. Idempotent. Called once by the serve
    /// loop after a SIGTERM, transport-close, or
    /// [`request_shutdown`](Self::request_shutdown) wakes the
    /// outer `tokio::select!`.
    ///
    /// The watcher loop polls `shutdown_flag` every ~500 ms, so the
    /// worst-case latency is one poll interval plus the current
    /// batch's processing time. A final `PRAGMA optimize` still runs
    /// from `Store::drop`.
    pub async fn shutdown(&self) {
        info!("shutdown: requested");
        self.shutdown_flag.store(true, Ordering::Relaxed);
        // Wake any in-flight `await_shutdown_request()` callers so they
        // don't sit on a dead future after the actual teardown runs.
        self.shutdown_notify.notify_waiters();

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

        // Persist lifetime telemetry before vacuum so the trailing
        // session window survives the exit. Synchronous — we WANT to
        // block on this write here, unlike the hot-path async flush.
        self.flush_telemetry_sync();

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

// Tool methods need to be `pub` so the multi-repo wrapper in
// `multi_repo.rs` can shim them (each shim is `self.active.code_outline(p)`
// etc). Their full prose lives in the `#[tool(description = "...")]`
// attribute, which is what agents and tool-search consume; the Rust
// doc-comment requirement is suppressed for the impl block to avoid
// duplicating that prose into a `///` line for every method.
#[allow(missing_docs)]
#[tool_router(router = tool_router)]
impl ReconServer {
    #[tool(
        name = "code_outline",
        description = "Show one-line-per-symbol outline of a file. Returns symbol kinds, names, and line numbers in a tree structure. Use instead of Read when you need to understand a file's structure without reading its full content. Typical output: 300-500 tokens for a 500-line file."
    )]
    pub async fn code_outline(&self, params: Parameters<OutlineParams>) -> String {
        self.instrumented_measured("code_outline", async move {
            // Validate path doesn't escape repo root. Capture the canonical
            // path so the measured-baseline read at the end can reuse it
            // (no second `resolve_path` round-trip on the success path).
            let canonical = match self.resolve_path(&params.0.path) {
                Ok(p) => p,
                Err((code, msg)) => {
                    return (
                        tool_error(
                            code,
                            msg,
                            Some(serde_json::json!({ "path": params.0.path })),
                        ),
                        None,
                    )
                }
            };
            let symbols = {
                let rel_path = PathBuf::from(&params.0.path);
                match self.read_pool.symbols_for_path(&rel_path) {
                    Ok(s) => s,
                    Err(e) => return (tool_error_from(&e), None),
                }
            };

            // Build a name->id map of top-level types (struct/enum/trait/class) so
            // we can rescue impl-block methods whose parent_id is missing (legacy
            // index rows pre-parser-fix) or pointing to the enclosing scope (impl
            // appears before its type in source).
            let type_id_by_name: AHashMap<&str, u64> = symbols
                .iter()
                .filter(|s| {
                    matches!(
                        s.kind,
                        recon_core::symbol::SymbolKind::Struct
                            | recon_core::symbol::SymbolKind::Enum
                            | recon_core::symbol::SymbolKind::Trait
                            | recon_core::symbol::SymbolKind::Class
                    )
                })
                .map(|s| (s.name.as_str(), s.id))
                .collect();

            // qualified_name "Type::method" rescue: maps a method back to its owning
            // type id when parent_id is None or the legacy 0 sentinel.
            let qname_rescue = |sym: &recon_core::symbol::Symbol| -> Option<u64> {
                sym.qualified_name
                    .as_str()
                    .split_once("::")
                    .and_then(|(ty, _)| {
                        let base = ty.split('<').next().unwrap_or(ty).trim();
                        type_id_by_name.get(base).copied()
                    })
            };

            // O(n) child lookup: build parent_id -> children map in one pass.
            // parent_id == Some(0) is a legacy sentinel meaning "no real parent" —
            // treat it as None for grouping purposes.
            let mut children_map: AHashMap<u64, Vec<&recon_core::symbol::Symbol>> = AHashMap::new();
            for sym in &symbols {
                let effective_parent = match sym.parent_id {
                    Some(0) | None => qname_rescue(sym),
                    Some(pid) => Some(pid),
                };
                if let Some(pid) = effective_parent {
                    children_map.entry(pid).or_default().push(sym);
                }
            }

            // A symbol is top-level if it has no effective parent.
            let mut entries = SmallVec::new();
            for sym in &symbols {
                let is_top_level = match sym.parent_id {
                    None | Some(0) => qname_rescue(sym).is_none(),
                    Some(_) => false,
                };
                if is_top_level {
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
            let response = redact_response(
                serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")),
            );
            // Measured baseline: token-cost of reading the file outright.
            // Only computed when `RECON_MEASURED_BASELINES=1` is set;
            // returns None silently when the file is absent or too big.
            let measured = self.measure_read_baseline(&canonical).await;
            (response, measured)
        })
        .await
    }

    #[tool(
        name = "code_skeleton",
        description = "Show signatures and docstrings with bodies elided as '...'. 10x compression vs full file read. Use instead of Read when you need to understand APIs and structure. Output: ~300 tokens per 3000-token file."
    )]
    pub async fn code_skeleton(&self, params: Parameters<SkeletonParams>) -> String {
        self.instrumented_measured("code_skeleton", async move {
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

            // When the indexer didn't produce a skeleton (typically a file
            // we can't parse), `code_skeleton` falls back to reading the
            // first 50 lines of the file. The full file content is also
            // the canonical "what would Read have cost" measurement, so
            // we capture the whole content here and reuse it both for
            // the truncated skeleton output and for the measured baseline.
            let mut measured_from_fallback: Option<u64> = None;
            if skeleton.is_empty() {
                let abs_path = match self.resolve_path(&params.0.path) {
                    Ok(p) => p,
                    Err((code, msg)) => {
                        return (
                            tool_error(
                                code,
                                msg,
                                Some(serde_json::json!({ "path": params.0.path })),
                            ),
                            None,
                        );
                    }
                };
                // Size cap to prevent OOM on large files (minified bundles, lock files, etc.)
                match tokio::fs::metadata(&abs_path).await {
                    Ok(m) if m.len() > MAX_READ_FILE_SIZE => {
                        return (
                            tool_error(
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
                            ),
                            None,
                        );
                    }
                    Err(e) => {
                        return (
                            tool_error(
                                ReconErrorCode::Io,
                                format!("reading file metadata: {e}"),
                                Some(serde_json::json!({ "path": params.0.path })),
                            ),
                            None,
                        );
                    }
                    _ => {}
                }
                let content = match tokio::fs::read_to_string(&abs_path).await {
                    Ok(c) => c,
                    Err(e) => {
                        return (
                            tool_error(
                                ReconErrorCode::Io,
                                format!("reading file: {e}"),
                                Some(serde_json::json!({ "path": params.0.path })),
                            ),
                            None,
                        );
                    }
                };
                measured_from_fallback =
                    Some(recon_search::tokens::estimate_tokens(&content) as u64);
                // Build the truncated preview in one pass to avoid the
                // intermediate `Vec<&str>` + `join` allocation. `content` is
                // already bounded by `MAX_READ_FILE_SIZE`, so capping the
                // capacity at 8 KB covers the 50-line preview comfortably.
                let mut buf = String::with_capacity(content.len().min(8 * 1024));
                for (i, line) in content.lines().take(50).enumerate() {
                    if i > 0 {
                        buf.push('\n');
                    }
                    buf.push_str(line);
                }
                skeleton = buf;
            }

            let token_est = recon_search::tokens::estimate_tokens(&skeleton);
            let view = ToolOutput::Skeleton(SkeletonView {
                path: Some(rel_path.clone()),
                content: skeleton,
                token_estimate: token_est,
            });
            let response = redact_response(
                serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")),
            );
            // Measured baseline: full Read of the file. If the fallback
            // path above already read it, reuse that token count; on the
            // happy path (skeleton came from the index) we re-read here
            // — only when the flag is on, gated inside the helper.
            let measured = match measured_from_fallback {
                Some(m) => Some(m),
                None => match self.resolve_path(&params.0.path) {
                    Ok(abs) => self.measure_read_baseline(&abs).await,
                    Err(_) => None,
                },
            };
            (response, measured)
        })
        .await
    }

    #[tool(
        name = "code_read_symbol",
        description = "Read the full source of one symbol plus its parent chain and caller/callee references. Use instead of Read when you need one specific function or type. Output: ~200-800 tokens."
    )]
    pub async fn code_read_symbol(&self, params: Parameters<ReadSymbolParams>) -> String {
        self.instrumented_measured("code_read_symbol", async move {
            let abs_path = match self.resolve_path(&params.0.path) {
                Ok(p) => p,
                Err((code, msg)) => {
                    return (
                        tool_error(
                            code,
                            msg,
                            Some(serde_json::json!({ "path": params.0.path })),
                        ),
                        None,
                    );
                }
            };
            // Size cap to prevent OOM on large files.
            match tokio::fs::metadata(&abs_path).await {
                Ok(m) if m.len() > MAX_READ_FILE_SIZE => {
                    return (
                        tool_error(
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
                        ),
                        None,
                    );
                }
                Err(e) => {
                    return (
                        tool_error(
                            ReconErrorCode::Io,
                            format!("reading file metadata: {e}"),
                            Some(serde_json::json!({ "path": params.0.path })),
                        ),
                        None,
                    );
                }
                _ => {}
            }
            let content = match tokio::fs::read_to_string(&abs_path).await {
                Ok(c) => c,
                Err(e) => {
                    return (
                        tool_error(
                            ReconErrorCode::Io,
                            format!("reading file: {e}"),
                            Some(serde_json::json!({ "path": params.0.path })),
                        ),
                        None,
                    );
                }
            };

            // Measured baseline: token-cost of the full file we just
            // read. Captured here so it's available on every successful
            // path below — we already paid the read I/O for our own work.
            let measured: Option<u64> =
                Some(recon_search::tokens::estimate_tokens(&content) as u64);

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
                    return (
                        tool_error(
                            ReconErrorCode::NotFound,
                            format!("symbol not found: {}", params.0.symbol_or_line),
                            Some(serde_json::json!({
                                "path": params.0.path,
                                "symbol_or_line": params.0.symbol_or_line,
                            })),
                        ),
                        // Even though the symbol wasn't found, the agent
                        // would still have paid the file Read to look —
                        // attribute the measured baseline to this call.
                        measured,
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
                context: None,
            });
            let response = redact_response(
                serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")),
            );
            (response, measured)
        })
        .await
    }

    #[tool(
        name = "code_find_symbol",
        description = "Find symbols by name across the codebase. Tiered: exact SQLite match -> Tantivy BM25 -> FTS5 trigram + nucleo fuzzy. Use instead of Grep when searching for functions, types, or classes."
    )]
    pub async fn code_find_symbol(&self, params: Parameters<FindSymbolParams>) -> String {
        self.instrumented("code_find_symbol", async move {
            // All reads go through lock-free ReadPool
            // Tier 0: exact match via SQLite index
            let mut results = self
                .read_pool
                .find_symbols_exact(&params.0.name, 20)
                .unwrap_or_default();

            // Tier 1: Tantivy BM25 structured search. Lock-free ≠ non-blocking —
            // the query parser + top-k collector is CPU-bound, so we offload
            // via `tantivy_search` so the tokio worker isn't held.
            if results.is_empty() {
                let hits = self.tantivy_search(params.0.name.clone(), 20).await;
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

            // Tier 3: Semantic embedding fallback. Always linked, but
            // a no-op when no embed service is initialised (no credentials,
            // RECON_NO_EMBED=1, hosted endpoint failed at startup).
            let mut from_embedding = false;
            if results.is_empty() {
                let svc = self.embed_service.read().clone();
                let pool = self.vec_read_pool.load_full();
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

            // Every retrieval tier caps at 20 — pass the cap so the
            // envelope carries `truncated: true` when a tier hits its limit.
            hits_response("symbol", entries, 20)
        })
        .await
    }

    #[tool(
        name = "code_find_refs",
        description = "Find all references to a symbol. Returns a count and top-k call sites as path:line triples. Use instead of Grep for finding usages of a function or type."
    )]
    pub async fn code_find_refs(&self, params: Parameters<FindRefsParams>) -> String {
        self.instrumented("code_find_refs", async move {
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

            // Filter orphan refs (no location row, or empty path) BEFORE the take(20)
            // cap so the digest doesn't fill up with degenerate {path:"", line:0}
            // entries from stale rows that lost their parent symbol.
            let valid: Vec<&recon_core::symbol::Ref> = refs
                .iter()
                .filter(|r| {
                    loc_map
                        .get(&r.src_symbol_id)
                        .is_some_and(|(p, _)| !p.is_empty())
                })
                .collect();

            let top_k: Vec<RefEntry> = valid
                .iter()
                .take(20)
                .map(|r| {
                    let (path, line) = loc_map
                        .get(&r.src_symbol_id)
                        .cloned()
                        .expect("filtered above");
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
                total: valid.len(),
                top_k,
                path: vec![],
                tiers: vec![],
                truncated: false,
                unresolved_hint: None,
                tests: vec![],
            });
            redact_response(serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")))
        })
        .await
    }

    #[tool(
        name = "code_path",
        description = "Shortest call-graph path from `src` to `dst`. Use to answer 'how does X reach Y?' \u{2014} replaces a chain of code_find_refs calls. Both arguments accept a bare name or a fully qualified name (preferred \u{2014} disambiguates). Returns an ordered hop sequence with file:line per hop. When unreachable within `max_hops` (default 8, max 16) returns an Error with kind 'not_found'/'unreachable' plus an `unresolved_hint` when the BFS hit a likely dyn-dispatch / FFI boundary. When src or dst is ambiguous (multiple symbols share the name) the BFS spans the cross-product and returns the shortest match. Bidirectional BFS over the cached reference graph; total-visit cap 50 000 nodes. Output uses ReferenceDigest with the `path` field populated."
    )]
    pub async fn code_path(&self, params: Parameters<PathParams>) -> String {
        self.instrumented("code_path", async move {
            let max_hops = params.0.max_hops.min(recon_search::graph::MAX_ALLOWED_HOPS);
            if max_hops == 0 {
                return tool_error(ReconErrorCode::InvalidParams, "max_hops must be >= 1", None);
            }
            let symbols = self.cached_all_symbols();
            let srcs = Self::resolve_symbol_to_indices(&symbols, &params.0.src);
            if srcs.is_empty() {
                return tool_error(
                    ReconErrorCode::NotFound,
                    format!("source symbol not found: {}", params.0.src),
                    Some(serde_json::json!({ "symbol": params.0.src })),
                );
            }
            let dsts = Self::resolve_symbol_to_indices(&symbols, &params.0.dst);
            if dsts.is_empty() {
                return tool_error(
                    ReconErrorCode::NotFound,
                    format!("destination symbol not found: {}", params.0.dst),
                    Some(serde_json::json!({ "symbol": params.0.dst })),
                );
            }
            let graph = self.cached_call_graph();
            let caps = recon_search::graph::GraphCaps::default_for_path(max_hops);
            let res = graph.shortest_path(&srcs, &dsts, &caps);
            match res {
                recon_search::graph::ShortestPathResult::Found { path } => {
                    let hops: Vec<recon_core::shapes::SymbolHop> = path
                        .iter()
                        .map(|&i| Self::symbol_hop_for_idx(&symbols, i))
                        .collect();
                    let view = ToolOutput::ReferenceDigest(RefDigestView {
                        symbol: params.0.src.as_str().into(),
                        total: hops.len(),
                        top_k: vec![],
                        path: hops,
                        tiers: vec![],
                        truncated: false,
                        unresolved_hint: None,
                        tests: vec![],
                    });
                    redact_response(
                        serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")),
                    )
                }
                recon_search::graph::ShortestPathResult::Unreachable { unresolved_near } => {
                    let hint = unresolved_near.map(|i| {
                        format!(
                            "unresolved boundary near {}",
                            symbols[i as usize].qualified_name
                        )
                    });
                    tool_error(
                        ReconErrorCode::NotFound,
                        "unreachable",
                        Some(serde_json::json!({
                            "src": params.0.src,
                            "dst": params.0.dst,
                            "max_hops": max_hops,
                            "unresolved_hint": hint,
                        })),
                    )
                }
                recon_search::graph::ShortestPathResult::VisitCapHit => tool_error(
                    ReconErrorCode::ResourceExhausted,
                    "shortest-path search exceeded the visit cap (50 000 nodes); narrow src or dst",
                    Some(serde_json::json!({
                        "src": params.0.src,
                        "dst": params.0.dst,
                    })),
                ),
            }
        })
        .await
    }

    #[tool(
        name = "code_callers",
        description = "Transitive callers of `symbol` up to `depth` rings (default 1, max 6). Replaces depth-many chained code_find_refs calls. Returns one tier per ring with the symbols at that depth. Cycle-safe (each symbol emitted at its minimum depth only). Per-tier fan-out is capped at 50 to bound god-node responses; total-visit cap 50 000 nodes. When either cap fires `truncated: true` is set. Returns symbol identities (qname + path + line of definition), not call-site lines \u{2014} use code_find_refs for the lexical call-site digest. `symbol` accepts bare or fully qualified names; ambiguous bare names traverse from all matches. Output uses ReferenceDigest with the `tiers` field populated."
    )]
    pub async fn code_callers(&self, params: Parameters<CallersParams>) -> String {
        self.instrumented("code_callers", async move {
            self.callers_or_callees_inner(params, true).await
        })
        .await
    }

    #[tool(
        name = "code_callees",
        description = "Transitive callees of `symbol` up to `depth` rings (default 1, max 6). Mirror of code_callers \u{2014} what does this symbol call (directly and transitively)? Cycle-safe, per-tier fan-out capped at 50, total-visit cap 50 000. `truncated: true` when caps fire. Returns symbol identities (qname + path + line of definition), not call-site lines. Use this to understand what changing X *requires* you to also understand (callees) versus what changing X *risks breaking* (callers). Output uses ReferenceDigest with the `tiers` field populated."
    )]
    pub async fn code_callees(&self, params: Parameters<CallersParams>) -> String {
        self.instrumented("code_callees", async move {
            self.callers_or_callees_inner(params, false).await
        })
        .await
    }

    #[tool(
        name = "code_context",
        description = "One-shot bundle of everything an agent needs to reason about a symbol \u{2014} replaces the canonical 4-call understand-X loop (find_symbol \u{2192} read_symbol \u{2192} find_refs \u{2192} search-for-tests). Returns: (1) the target symbol's signature + doc + first ~20 body lines, (2) up to 5 immediate callers, (3) up to 5 immediate callees, (4) up to 3 referenced types, (5) up to 3 tests that exercise it. Honors `token_budget` (default 2000); drops sections under pressure in this order: tests \u{2192} callees \u{2192} types \u{2192} callers (skeleton+body always kept). Accepts a bare name or a fully qualified name. When ambiguous (multiple symbols share the bare name) returns an Error with kind 'invalid_params' listing up to 5 candidates; reissue with a qualified name. Output uses SymbolCard with the `context` envelope populated. Test detection in v0.3 is Rust-only (tests::* qname patterns and test_* / Test* function names); cross-language coverage is on the v0.4 roadmap."
    )]
    pub async fn code_context(&self, params: Parameters<ContextParams>) -> String {
        self.instrumented_measured("code_context", async move {
            let symbols = self.cached_all_symbols();
            let matches = Self::resolve_symbol_to_indices(&symbols, &params.0.symbol);
            match matches.len() {
                0 => {
                    return (
                        tool_error(
                            ReconErrorCode::NotFound,
                            format!("symbol not found: {}", params.0.symbol),
                            Some(serde_json::json!({ "symbol": params.0.symbol })),
                        ),
                        None,
                    );
                }
                1 => {}
                n => {
                    let candidates: Vec<recon_core::shapes::SymbolHop> = matches
                        .iter()
                        .take(5)
                        .map(|&i| Self::symbol_hop_for_idx(&symbols, i))
                        .collect();
                    return (
                        tool_error(
                            ReconErrorCode::InvalidParams,
                            format!(
                                "ambiguous symbol: {n} candidates share the name '{}'; reissue with a fully qualified name",
                                params.0.symbol
                            ),
                            Some(serde_json::json!({
                                "symbol": params.0.symbol,
                                "candidates": candidates,
                            })),
                        ),
                        None,
                    );
                }
            }

            let target_idx = matches[0];
            let target = symbols[target_idx as usize].clone();
            let abs_path = match self.resolve_path(target.path.to_string_lossy().as_ref()) {
                Ok(p) => p,
                Err((code, msg)) => {
                    return (
                        tool_error(
                            code,
                            msg,
                            Some(serde_json::json!({ "path": target.path.to_string_lossy() })),
                        ),
                        None,
                    );
                }
            };

            let content = match tokio::fs::metadata(&abs_path).await {
                Ok(m) if m.len() > MAX_READ_FILE_SIZE => {
                    return (
                        tool_error(
                            ReconErrorCode::FileTooLarge,
                            format!(
                                "file too large ({} MB, max {} MB)",
                                m.len() / (1024 * 1024),
                                MAX_READ_FILE_SIZE / (1024 * 1024)
                            ),
                            Some(serde_json::json!({
                                "path": target.path.to_string_lossy(),
                                "size_bytes": m.len(),
                            })),
                        ),
                        None,
                    );
                }
                Err(e) => {
                    return (
                        tool_error(
                            ReconErrorCode::Io,
                            format!("reading file metadata: {e}"),
                            Some(serde_json::json!({ "path": target.path.to_string_lossy() })),
                        ),
                        None,
                    );
                }
                Ok(_) => match tokio::fs::read_to_string(&abs_path).await {
                    Ok(c) => c,
                    Err(e) => {
                        return (
                            tool_error(
                                ReconErrorCode::Io,
                                format!("reading file: {e}"),
                                Some(serde_json::json!({ "path": target.path.to_string_lossy() })),
                            ),
                            None,
                        );
                    }
                },
            };

            // Measured baseline: the target's full file content. The
            // agent's full alternative loop (read_symbol → find_refs →
            // search-tests) costs strictly more than this — the file
            // read is the dominant component, the rest is grep
            // overhead. Reporting the file-read floor keeps the
            // measurement honest (under-counts rather than inflates).
            let measured_baseline =
                Some(recon_search::tokens::estimate_tokens(&content) as u64);

            let line_start = *target.line_range.start() as usize;
            let line_end = *target.line_range.end() as usize;
            let body_lines: Vec<&str> = content
                .lines()
                .skip(line_start.saturating_sub(1))
                .take(line_end.saturating_sub(line_start.saturating_sub(1)).min(20))
                .collect();
            let body = body_lines.join("\n");

            let graph = self.cached_call_graph();
            let caller_caps = recon_search::graph::GraphCaps::default_for_callers(1);
            let callers_result = graph.transitive_callers(&[target_idx], &caller_caps);
            let callees_result = graph.transitive_callees(&[target_idx], &caller_caps);

            let callers_hops: Vec<recon_core::shapes::SymbolHop> = callers_result
                .tiers
                .first()
                .map(|t| {
                    t.nodes
                        .iter()
                        .take(5)
                        .map(|&i| Self::symbol_hop_for_idx(&symbols, i))
                        .collect()
                })
                .unwrap_or_default();

            let mut all_callee_idxs: Vec<u32> = callees_result
                .tiers
                .first()
                .map(|t| t.nodes.clone())
                .unwrap_or_default();
            let type_idxs: Vec<u32> = all_callee_idxs
                .iter()
                .copied()
                .filter(|&i| {
                    let k = symbols[i as usize].kind;
                    matches!(
                        k,
                        recon_core::symbol::SymbolKind::Struct
                            | recon_core::symbol::SymbolKind::Class
                            | recon_core::symbol::SymbolKind::Trait
                            | recon_core::symbol::SymbolKind::Enum
                            | recon_core::symbol::SymbolKind::Type
                            | recon_core::symbol::SymbolKind::Interface
                    )
                })
                .take(3)
                .collect();
            let type_set: ahash::AHashSet<u32> = type_idxs.iter().copied().collect();
            all_callee_idxs.retain(|i| !type_set.contains(i));
            let callee_hops: Vec<recon_core::shapes::SymbolHop> = all_callee_idxs
                .iter()
                .take(5)
                .map(|&i| Self::symbol_hop_for_idx(&symbols, i))
                .collect();
            let type_hops: Vec<recon_core::shapes::SymbolHop> = type_idxs
                .iter()
                .map(|&i| Self::symbol_hop_for_idx(&symbols, i))
                .collect();

            let test_caps = recon_search::graph::GraphCaps::default_for_callers(4);
            let test_callers = graph.transitive_callers(&[target_idx], &test_caps);
            let mut test_hops: Vec<recon_core::shapes::SymbolHop> = Vec::with_capacity(3);
            'outer: for tier in &test_callers.tiers {
                for &i in &tier.nodes {
                    if Self::is_phase1_test_symbol(&symbols[i as usize]) {
                        test_hops.push(Self::symbol_hop_for_idx(&symbols, i));
                        if test_hops.len() >= 3 {
                            break 'outer;
                        }
                    }
                }
            }

            let mut envelope = recon_core::shapes::ContextEnvelope {
                callers: callers_hops,
                callees: vec![],
                types: vec![],
                tests: vec![],
                truncated: false,
            };

            let target_card_size = recon_search::tokens::estimate_tokens(&body)
                + target
                    .signature
                    .as_deref()
                    .map(recon_search::tokens::estimate_tokens)
                    .unwrap_or(0)
                + target
                    .doc
                    .as_deref()
                    .map(recon_search::tokens::estimate_tokens)
                    .unwrap_or(0);

            let mut spent = target_card_size
                + envelope
                    .callers
                    .iter()
                    .map(|h| recon_search::tokens::estimate_tokens(&h.qualified_name))
                    .sum::<usize>();
            let budget = params.0.token_budget;

            for hop in type_hops {
                let est = recon_search::tokens::estimate_tokens(&hop.qualified_name) + 6;
                if spent + est > budget {
                    envelope.truncated = true;
                    break;
                }
                spent += est;
                envelope.types.push(hop);
            }
            for hop in callee_hops {
                let est = recon_search::tokens::estimate_tokens(&hop.qualified_name) + 6;
                if spent + est > budget {
                    envelope.truncated = true;
                    break;
                }
                spent += est;
                envelope.callees.push(hop);
            }
            for hop in test_hops {
                let est = recon_search::tokens::estimate_tokens(&hop.qualified_name) + 6;
                if spent + est > budget {
                    envelope.truncated = true;
                    break;
                }
                spent += est;
                envelope.tests.push(hop);
            }

            let parent_chain = Self::parent_chain_for(&symbols, target_idx);

            let view = ToolOutput::SymbolCard(SymbolCardView {
                path: (*target.path).clone(),
                qualified_name: target.qualified_name.to_string(),
                kind: target.kind,
                signature: target.signature.as_deref().map(str::to_owned),
                doc: target.doc.as_deref().map(str::to_owned),
                body,
                line_range: (*target.line_range.start(), *target.line_range.end()),
                parent_chain,
                callers: vec![],
                callees: vec![],
                context: Some(envelope),
            });
            let response = redact_response(
                serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")),
            );
            (response, measured_baseline)
        })
        .await
    }

    #[tool(
        name = "code_impact",
        description = "Blast radius of changing `symbol` \u{2014} transitive callers up to `depth` rings (default 4, max 6) plus tests that exercise it. Returns one tier per ring (production callers), a separate `tests` array for transitively-reaching test functions (Rust-only Phase-1 detector: tests::* qnames + test_* / Test* names), and `truncated: true` when fan-out caps fire. Use to answer 'what might break if I change X?' before refactoring. Per-tier fan-out cap 50, total-visit cap 50 000 \u{2014} a god-node query terminates with a marker rather than blowing up. Output uses ReferenceDigest with the `tiers` and `tests` fields populated."
    )]
    pub async fn code_impact(&self, params: Parameters<ImpactParams>) -> String {
        self.instrumented("code_impact", async move {
            let depth = params.0.depth;
            if depth == 0 {
                return tool_error(ReconErrorCode::InvalidParams, "depth must be >= 1", None);
            }
            let depth = depth.min(recon_search::graph::MAX_ALLOWED_DEPTH);
            let symbols = self.cached_all_symbols();
            let seeds = Self::resolve_symbol_to_indices(&symbols, &params.0.symbol);
            if seeds.is_empty() {
                return tool_error(
                    ReconErrorCode::NotFound,
                    format!("symbol not found: {}", params.0.symbol),
                    Some(serde_json::json!({ "symbol": params.0.symbol })),
                );
            }
            let graph = self.cached_call_graph();
            let caps = recon_search::graph::GraphCaps::default_for_callers(depth);
            let result = graph.transitive_callers(&seeds, &caps);

            let mut tests: Vec<recon_core::shapes::SymbolHop> = Vec::new();
            let mut seen_test_idx: ahash::AHashSet<u32> = ahash::AHashSet::new();
            let prod_tiers: Vec<recon_core::shapes::RefTier> = result
                .tiers
                .iter()
                .map(|t| {
                    let mut prod_nodes: Vec<u32> = Vec::with_capacity(t.nodes.len());
                    for &i in &t.nodes {
                        if Self::is_phase1_test_symbol(&symbols[i as usize]) {
                            if seen_test_idx.insert(i) {
                                tests.push(Self::symbol_hop_for_idx(&symbols, i));
                            }
                        } else {
                            prod_nodes.push(i);
                        }
                    }
                    recon_core::shapes::RefTier {
                        depth: t.depth,
                        refs: prod_nodes
                            .iter()
                            .map(|&i| Self::symbol_hop_for_idx(&symbols, i))
                            .collect(),
                        truncated: t.truncated_at_cap,
                    }
                })
                .collect();

            let total: usize = prod_tiers.iter().map(|t| t.refs.len()).sum::<usize>() + tests.len();

            let view = ToolOutput::ReferenceDigest(RefDigestView {
                symbol: params.0.symbol.as_str().into(),
                total,
                top_k: vec![],
                path: vec![],
                tiers: prod_tiers,
                truncated: result.truncated,
                unresolved_hint: None,
                tests,
            });
            redact_response(serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")))
        })
        .await
    }

    #[tool(
        name = "code_subsystems",
        description = "List the natural subsystems of the repo \u{2014} weakly-connected components of the reference graph. Each subsystem has an id (use with code_subsystem), the qualified-name of its highest-degree symbol (the 'hub'), the dominant directory, and a symbol count. Use to orient yourself before drilling in: subsystems separate cleanly along architectural lines (e.g. recon-search vs recon-storage) without you having to know the directory structure. Sorted by symbol count descending. `limit` caps the number returned (default 50). Output uses Skeleton with subsystems rendered as one line each. Phase 2 v0.3.x: connected components only. Future v0.4.x adds Leiden modularity-optimized clustering."
    )]
    pub async fn code_subsystems(&self, params: Parameters<SubsystemsParams>) -> String {
        self.instrumented("code_subsystems", async move {
            let symbols = self.cached_all_symbols();
            let graph = self.cached_call_graph();
            let comps = graph.connected_components();

            let mut buckets: ahash::AHashMap<u32, Vec<u32>> = ahash::AHashMap::new();
            for (i, &cid) in comps.iter().enumerate() {
                buckets.entry(cid).or_default().push(i as u32);
            }

            let mut summaries: Vec<(u32, u32, u32, String, String)> =
                Vec::with_capacity(buckets.len());
            for (cid, members) in buckets {
                let hub_idx = members
                    .iter()
                    .filter(|&&i| symbols[i as usize].parent_id.is_none())
                    .max_by_key(|&&i| graph.in_degree(i) + graph.out_degree(i))
                    .copied()
                    .unwrap_or_else(|| members[0]);
                let hub_qname = symbols[hub_idx as usize].qualified_name.to_string();
                let mut dir_counts: ahash::AHashMap<String, u32> = ahash::AHashMap::new();
                for &i in &members {
                    let p = symbols[i as usize].path.to_string_lossy();
                    let dir = p.split('/').take(3).collect::<Vec<_>>().join("/");
                    *dir_counts.entry(dir).or_default() += 1;
                }
                let dominant_dir = dir_counts
                    .into_iter()
                    .max_by_key(|(_, c)| *c)
                    .map(|(k, _)| k)
                    .unwrap_or_default();
                summaries.push((cid, members.len() as u32, hub_idx, hub_qname, dominant_dir));
            }
            summaries.sort_by_key(|s| std::cmp::Reverse(s.1));
            summaries.truncate(params.0.limit);

            let mut content = String::with_capacity(summaries.len() * 80);
            content.push_str("# subsystems (id : count : hub : dir)\n");
            for (cid, count, _hub_idx, hub_qname, dir) in &summaries {
                content.push_str(&format!("{cid}\t{count}\t{hub_qname}\t{dir}\n"));
            }

            let view = ToolOutput::Skeleton(recon_core::shapes::SkeletonView {
                path: None,
                content: content.clone(),
                token_estimate: recon_search::tokens::estimate_tokens(&content),
            });
            redact_response(serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")))
        })
        .await
    }

    #[tool(
        name = "code_subsystem",
        description = "Detailed view of one subsystem (from code_subsystems). Returns a skeleton-style summary of all symbols in the component \u{2014} qname, kind, file:line \u{2014} within `token_budget` tokens (default 1500). Use after code_subsystems to drill into a specific cluster without reading every file in the directory. Output uses Skeleton."
    )]
    pub async fn code_subsystem(&self, params: Parameters<SubsystemParams>) -> String {
        self.instrumented("code_subsystem", async move {
            let symbols = self.cached_all_symbols();
            let graph = self.cached_call_graph();
            let comps = graph.connected_components();
            let target_id = params.0.id;

            let mut members: Vec<u32> = comps
                .iter()
                .enumerate()
                .filter(|(_, &cid)| cid == target_id)
                .map(|(i, _)| i as u32)
                .collect();
            if members.is_empty() {
                return tool_error(
                    ReconErrorCode::NotFound,
                    format!("subsystem not found: {target_id}"),
                    Some(serde_json::json!({ "id": target_id })),
                );
            }
            members.sort_by_key(|&i| std::cmp::Reverse(graph.in_degree(i) + graph.out_degree(i)));

            let mut content = String::with_capacity(params.0.token_budget * 4);
            let mut tokens: usize = 0;
            for idx in members {
                let s = &symbols[idx as usize];
                let line = format!(
                    "{}:{} {} {}",
                    s.path.to_string_lossy(),
                    s.line_range.start(),
                    s.kind.label(),
                    s.qualified_name
                );
                let est = recon_search::tokens::estimate_tokens(&line) + 1;
                if tokens + est > params.0.token_budget {
                    break;
                }
                content.push_str(&line);
                content.push('\n');
                tokens += est;
            }

            let view = ToolOutput::Skeleton(recon_core::shapes::SkeletonView {
                path: None,
                content,
                token_estimate: tokens,
            });
            redact_response(serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")))
        })
        .await
    }

    /// Per-tool token-savings breakdown. **CLI-only** — invoked from
    /// `recon savings show` via [`Self::query_tool`]; intentionally
    /// not registered as an MCP tool (no `#[tool(...)]` attribute) so
    /// agents don't burn context introspecting their own savings.
    /// Users get the same data through the CLI and the dashboard.
    pub async fn code_savings(&self, _params: Parameters<SavingsParams>) -> String {
        self.instrumented("code_savings", async move {
            let mut content = String::from(
                "# tool\tcalls\tresponse_tokens\tbaseline\ttokens_saved\tavg_latency_ms\n",
            );
            for (name, snapshot) in self.telemetry.per_tool_snapshots() {
                if snapshot.calls == 0 {
                    continue;
                }
                // `baseline` here is the sum of static + measured —
                // exactly one of the two contributes per call, so the
                // sum is the per-tool baseline credit regardless of
                // whether the tool is on the measured path.
                let baseline = snapshot
                    .static_baseline_tokens
                    .saturating_add(snapshot.measured_baseline_tokens);
                content.push_str(&format!(
                    "{name}\t{calls}\t{resp}\t{base}\t{saved}\t{latency:.2}\n",
                    name = name,
                    calls = snapshot.calls,
                    resp = snapshot.response_tokens,
                    base = baseline,
                    saved = snapshot.tokens_saved(),
                    latency = snapshot.avg_latency_ms(),
                ));
            }
            // Aggregate trailer.
            let agg = self.telemetry.aggregate();
            let agg_baseline = agg
                .static_baseline_tokens
                .saturating_add(agg.measured_baseline_tokens);
            content.push_str(&format!(
                "# total\t{calls}\t{resp}\t{base}\t{saved}\t-\n",
                calls = agg.calls,
                resp = agg.response_tokens,
                base = agg_baseline,
                saved = agg.tokens_saved(),
            ));

            let view = ToolOutput::Skeleton(recon_core::shapes::SkeletonView {
                path: None,
                content: content.clone(),
                token_estimate: recon_search::tokens::estimate_tokens(&content),
            });
            redact_response(serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")))
        })
        .await
    }

    #[tool(
        name = "code_search",
        description = "Search for text patterns. Modes: exact (default), regex, hybrid (BM25 + text fused via reciprocal rank fusion). Use instead of Grep for code search."
    )]
    pub async fn code_search(&self, params: Parameters<SearchParams>) -> String {
        self.instrumented_measured("code_search", async move {
            let paths = self.cached_file_paths();

            let abs_paths = self
                .resolve_search_scope_async(&paths, params.0.filter.as_deref())
                .await;

            // Semantic mode — embed the query and search the local vector store.
            // No measured baseline: there's no plain grep alternative for vector
            // search, so the call doesn't accrue savings credit on either side.
            // Backend (hosted vs local) is selected at compile time by the
            // `local-embed` feature flag; this code path is identical either way.
            if params.0.mode == "semantic" {
                let svc = self.embed_service.read().clone();
                let pool = self.vec_read_pool.load_full();
                match (svc, pool) {
                    (Some(svc), Some(pool)) => {
                        // embed_one is sync and may block on HTTP (hosted) or
                        // ONNX (local); spawn_blocking so the tokio executor
                        // stays responsive either way.
                        let query = params.0.query.clone();
                        let query_vec = match tokio::task::spawn_blocking(move || {
                            svc.embed_one(&query)
                        })
                        .await
                        {
                            Ok(Ok(v)) => v,
                            Ok(Err(e)) => return (format!("embed error: {e}"), None),
                            Err(e) => return (format!("embed task join error: {e}"), None),
                        };
                        let results = match pool.search(query_vec, None, 20) {
                            Ok(r) => r,
                            Err(e) => return (format!("vector search error: {e}"), None),
                        };
                        let entries: Vec<serde_json::Value> = results
                            .iter()
                            .map(
                                |(id, dist)| serde_json::json!({"symbol_id": id, "distance": dist}),
                            )
                            .collect();
                        return (hits_response("text", entries, 20), None);
                    }
                    _ => return (
                        "semantic search requires the embed service — run `recon login <key>`, \
                         or set `RECON_NO_EMBED=1` to disable this mode and fall back to lexical."
                            .into(),
                        None,
                    ),
                }
            }

            if params.0.mode == "hybrid" {
                // RRF fusion: Tantivy BM25 results + text grep results.
                // Measured baseline reflects only the grep half — the BM25
                // half is index-driven with no clean Read+grep alternative.
                let tantivy_hits = self.tantivy_search(params.0.query.clone(), 20).await;
                let q = TextQuery {
                    pattern: params.0.query.clone(),
                    is_regex: false,
                    max_results: 20,
                    scope: abs_paths.clone(),
                };
                let (text_hits, measured) =
                    self.text_searcher.search_measured(&q).unwrap_or_default();

                // BTreeMap (not AHashMap) so iteration order is lexicographic by key.
                // Keys are deterministic strings (`{path}:{name}` or `{path}:{line}`),
                // so identical inputs produce byte-identical outputs — important for
                // prompt-cache hits when the agent re-issues the same hybrid query.
                // With AHashMap, ties on RRF score would be broken by hash iteration
                // order, which varies across runs.
                let mut rrf: std::collections::BTreeMap<String, (f64, serde_json::Value)> =
                    std::collections::BTreeMap::new();
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

                return (hits_response("text", entries, 30), Some(measured));
            }

            if params.0.mode == "regex" {
                let q = TextQuery {
                    pattern: params.0.query.clone(),
                    is_regex: true,
                    max_results: 30,
                    scope: abs_paths,
                };
                let (hits, measured) = self.text_searcher.search_measured(&q).unwrap_or_default();

                let entries: Vec<serde_json::Value> = hits
                    .iter()
                    .map(|h| {
                        let rel = h.path.strip_prefix(&self.repo_root).unwrap_or(&h.path);
                        text_hit_json(rel.to_string_lossy(), h.line, h.col, h.line_text.as_str())
                    })
                    .collect();

                return (hits_response("text", entries, 30), Some(measured));
            }

            // Exact mode: try Tantivy first (sub-ms), fall back to grep only if empty.
            // Tantivy-served calls don't run a grep pass, so the measured
            // baseline is the agent's *alternative* path: grep for the query
            // across the repo, then read the top-2 hit files. We approximate
            // that with the file content of the top-2 tantivy hits — same
            // rationale as the v0.3.x static estimate ("Grep + read 2 hit
            // files"), but per-call against real bytes.
            let tantivy_hits = self.tantivy_search(params.0.query.clone(), 30).await;
            if !tantivy_hits.is_empty() {
                // Tantivy hits carry symbol_id but no line number; resolve symbol
                // line_start in one batched query so callers see real line numbers.
                // Falls back to 0 only when the symbol row vanished mid-query.
                let ids: Vec<u64> = tantivy_hits.iter().map(|h| h.symbol_id).collect();
                let lines: AHashMap<u64, u32> = self
                    .read_pool
                    .symbol_locations_by_ids(&ids)
                    .map(|rows| rows.into_iter().map(|(id, _, line)| (id, line)).collect())
                    .unwrap_or_default();

                // Measured baseline: sum content tokens of up to 2 unique top
                // hit files. Reuses `measure_read_baseline` so the read uses
                // the same MAX_READ_FILE_SIZE cap real Read-shaped tools see.
                let mut measured: u64 = 0;
                let mut seen: ahash::AHashSet<&str> = ahash::AHashSet::new();
                for hit in tantivy_hits.iter() {
                    if seen.len() >= 2 {
                        break;
                    }
                    if !seen.insert(hit.path.as_str()) {
                        continue;
                    }
                    if let Ok(abs) = self.resolve_path(hit.path.as_str()) {
                        if let Some(t) = self.measure_read_baseline(&abs).await {
                            measured = measured.saturating_add(t);
                        }
                    }
                }

                let entries: Vec<serde_json::Value> = tantivy_hits
                    .iter()
                    .map(|hit| {
                        let line = lines.get(&hit.symbol_id).copied().unwrap_or(0);
                        text_hit_json(
                            hit.path.as_str(),
                            line,
                            None,
                            hit.signature.as_deref().unwrap_or(hit.name.as_str()),
                        )
                    })
                    .collect();
                return (hits_response("text", entries, 30), Some(measured));
            }

            // Tantivy had no hits — fall back to text grep, which gets a measured baseline.
            let q = TextQuery {
                pattern: params.0.query.clone(),
                is_regex: false,
                max_results: 30,
                scope: abs_paths,
            };
            let (hits, measured) = self.text_searcher.search_measured(&q).unwrap_or_default();

            let entries: Vec<serde_json::Value> = hits
                .iter()
                .map(|h| {
                    let rel = h.path.strip_prefix(&self.repo_root).unwrap_or(&h.path);
                    text_hit_json(rel.to_string_lossy(), h.line, h.col, h.line_text.as_str())
                })
                .collect();

            (hits_response("text", entries, 30), Some(measured))
        })
        .await
    }

    #[tool(
        name = "code_list",
        description = "List indexed source files with language, line count, and top symbols. Use instead of Glob when you need structured file listings. Supports language filter."
    )]
    pub async fn code_list(&self, params: Parameters<ListParams>) -> String {
        self.instrumented_measured("code_list", async move {
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
                                .filter_map(|p| {
                                    p.strip_prefix(&self.repo_root).ok().map(PathBuf::from)
                                })
                                .collect::<Vec<_>>()
                        })
                } else {
                    None
                }
            });

            let mut entries: Vec<serde_json::Value> = Vec::with_capacity(summaries.len());
            // Measured baseline: the agent's real `code_list` alternative is
            // `Glob + cat top-N files` to orient — not just enumerating
            // paths. v0.4.0 only summed (path + lang) bytes, which
            // under-counted by ~50× vs the static estimate (advisor flag).
            // v0.4.1 also tracks the top-3 kept paths so we can read their
            // content as the dominant component of the alternative cost.
            let mut measured_bytes: usize = 0;
            let mut top_paths: SmallVec<[PathBuf; 3]> = SmallVec::new();
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

                let path_str = path.to_string_lossy();
                measured_bytes += path_str.len() + lang.name().len() + 2;
                if top_paths.len() < 3 {
                    top_paths.push(path.clone());
                }
                entries.push(serde_json::json!({
                    "path": path_str, "lang": lang.name(),
                    "symbol_count": sym_count, "top_symbols": top_syms,
                }));
            }

            // Path-listing tokens (cheap), plus content of up to 3 top files
            // (the dominant cost — what an agent without recon would have
            // actually read after globbing).
            let mut measured_total: u64 = measured_bytes.div_ceil(4) as u64;
            for rel in &top_paths {
                if let Ok(abs) = self.resolve_path(rel.to_string_lossy().as_ref()) {
                    if let Some(t) = self.measure_read_baseline(&abs).await {
                        measured_total = measured_total.saturating_add(t);
                    }
                }
            }
            let measured = Some(measured_total);
            // `code_list` has no built-in row cap (it streams everything that
            // passes the filters), so pass `usize::MAX` as a sentinel — the
            // `truncated` flag is always omitted from the wire.
            let response = hits_response("file", entries, usize::MAX);
            (response, measured)
        })
        .await
    }

    #[tool(
        name = "code_repo_map",
        description = "Generate a ranked overview of the most important symbols in the repo. Uses personalized PageRank over the reference graph with Aider-style edge weights. Output fits within a token budget (default 2000). Best first tool to call for orientation."
    )]
    pub async fn code_repo_map(&self, params: Parameters<RepoMapParams>) -> String {
        self.instrumented("code_repo_map", async move {
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

            // PageRank (50-iter power iteration over the full ref graph) and the
            // subsequent render walk are both CPU-bound. On a cold call they can
            // run 50-200 ms, long enough to visibly stall any other tool call
            // landing on the same tokio worker thread. Offload to the blocking
            // pool; the `all_symbols`/`all_refs` clones are Arc bumps, not deep
            // copies.
            let content = {
                let all_symbols = all_symbols.clone();
                let all_refs = all_refs.clone();
                tokio::task::spawn_blocking(move || {
                    let ranked = pagerank::pagerank(
                        &all_symbols,
                        &all_refs,
                        &focus_indices,
                        0.85,
                        pagerank::DEFAULT_MAX_ITERATIONS,
                    );
                    pagerank::render_repo_map(&all_symbols, &ranked, budget)
                })
                .await
                .unwrap_or_default()
            };

            let token_est = recon_search::tokens::estimate_tokens(&content);
            let view = ToolOutput::Skeleton(SkeletonView {
                path: None,
                content,
                token_estimate: token_est,
            });
            let result = redact_response(
                serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")),
            );

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
        })
        .await
    }

    #[tool(
        name = "code_find_strings",
        description = "Search for patterns in string literals and comments. Finds SQL fragments, i18n keys, log messages that structural search misses."
    )]
    pub async fn code_find_strings(&self, params: Parameters<FindStringsParams>) -> String {
        self.instrumented_measured("code_find_strings", async move {
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
            // Measured: total tokens an unbounded grep would have emitted
            // for this pattern, capped per-call. Reuses the same scan pass
            // as the truncated hits — no extra I/O.
            let (hits, measured) = self.text_searcher.search_measured(&q).unwrap_or_default();

            // Pull the requested kind once so per-hit classification can filter.
            // "both" (default) keeps every hit; "literal" / "comment" filter to
            // hits whose match position on its line falls inside that context.
            let want = params.0.kind.as_str();
            let entries: Vec<serde_json::Value> = hits
                .iter()
                .filter_map(|h| {
                    let classified = classify_string_hit(&h.line_text, &params.0.pattern);
                    let keep = match want {
                        "literal" => classified == StringHitKind::Literal,
                        "comment" => classified == StringHitKind::Comment,
                        _ => true, // "both" or anything else → no filter
                    };
                    if !keep {
                        return None;
                    }
                    let rel = h.path.strip_prefix(&self.repo_root).unwrap_or(&h.path);
                    Some(serde_json::json!({
                        "path": rel.to_string_lossy(),
                        "line": h.line,
                        "text": h.line_text,
                        "kind": classified.as_str(),
                    }))
                })
                .collect();

            // Underlying grep caps `max_results` at 30; that's the truncation
            // signal even after the literal/comment classifier filters out a
            // few hits, since the cap was already applied upstream.
            let response = hits_response("string", entries, 30);
            (response, Some(measured))
        })
        .await
    }

    #[tool(
        name = "code_multi_find",
        description = "Search for multiple patterns at once. More efficient than multiple code_search calls. Returns results grouped by pattern."
    )]
    pub async fn code_multi_find(&self, params: Parameters<MultiFindParams>) -> String {
        self.instrumented_measured("code_multi_find", async move {
        let paths = self.cached_file_paths();

        let abs_paths = self
            .resolve_search_scope_async(&paths, params.0.filter.as_deref())
            .await;
        let pat_refs: Vec<&str> = params.0.patterns.iter().map(|s| s.as_str()).collect();
        let (multi_results, measured) = self
            .text_searcher
            .multi_search_measured(&pat_refs, &abs_paths, 10)
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

        // Multi-find returns one row per pattern; row-cap doesn't apply to
        // the pattern dimension. Pass `usize::MAX` so `truncated` is omitted.
        let response = hits_response("multi_find", results, usize::MAX);
        (response, Some(measured))
            }).await
    }

    #[tool(
        name = "code_reindex",
        description = "Trigger a full re-index of the repository. Use when you suspect the index is stale or after major file changes outside the editor."
    )]
    pub async fn code_reindex(&self, params: Parameters<ReindexParams>) -> String {
        self.instrumented("code_reindex", async move {
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
                    // Full reindex: clear existing data first.
                    // One bulk transaction instead of N per-file deletes (was a
                    // multi-second hot spot on large repos due to per-file WAL
                    // fsyncs).
                    info!("force reindex: clearing existing data");
                    {
                        let store = write_store.lock();
                        if let Err(e) = store.delete_all_files_cascade() {
                            warn!("force reindex: bulk clear failed: {e}");
                        }
                        if let Some(ref mut writer) = tantivy_writer.lock().as_mut() {
                            let _ = tantivy.commit(writer);
                        }
                    }

                    // Full walk + parse (force path)
                    let paths = walker::walk_repo(&repo_root);
                    let pools = std::sync::Arc::new(LanguagePools::new(
                        rayon::current_num_threads().max(4),
                    ));
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
        })
        .await
    }

    /// Index-health report. **CLI-only** — invoked from `recon stats`
    /// via [`Self::query_tool`]; intentionally not registered as an
    /// MCP tool (no `#[tool(...)]` attribute) so agents don't burn
    /// context on operator-level diagnostics. The dashboard surfaces
    /// the same numbers for end users.
    pub async fn code_stats(&self, _params: Parameters<StatsParams>) -> String {
        self.instrumented("code_stats", async move {
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

            // Centrality top-N from the cached call graph. Degree-based for
            // v0.3.x; PageRank/betweenness columns deferred to v0.4.x with
            // the index-time pass.
            let symbols = self.cached_all_symbols();
            let graph = self.cached_call_graph();
            const TOP_N: usize = 20;
            let mut by_in_degree: Vec<(u32, u32)> = (0..graph.n as u32)
                .filter(|&i| {
                    symbols
                        .get(i as usize)
                        .is_some_and(|s| s.parent_id.is_none())
                })
                .map(|i| (i, graph.in_degree(i)))
                .filter(|(_, d)| *d > 0)
                .collect();
            by_in_degree.sort_by_key(|x| std::cmp::Reverse(x.1));
            by_in_degree.truncate(TOP_N);
            let top_in_degree: Vec<serde_json::Value> = by_in_degree
                .iter()
                .map(|(idx, deg)| {
                    let s = &symbols[*idx as usize];
                    serde_json::json!({
                        "qualified_name": s.qualified_name.as_str(),
                        "kind": s.kind.label(),
                        "path": s.path.to_string_lossy(),
                        "line": s.line_range.start(),
                        "in_degree": deg,
                    })
                })
                .collect();

            let mut by_out_degree: Vec<(u32, u32)> = (0..graph.n as u32)
                .filter(|&i| {
                    symbols
                        .get(i as usize)
                        .is_some_and(|s| s.parent_id.is_none())
                })
                .map(|i| (i, graph.out_degree(i)))
                .filter(|(_, d)| *d > 0)
                .collect();
            by_out_degree.sort_by_key(|x| std::cmp::Reverse(x.1));
            by_out_degree.truncate(TOP_N);
            let top_out_degree: Vec<serde_json::Value> = by_out_degree
                .iter()
                .map(|(idx, deg)| {
                    let s = &symbols[*idx as usize];
                    serde_json::json!({
                        "qualified_name": s.qualified_name.as_str(),
                        "kind": s.kind.label(),
                        "path": s.path.to_string_lossy(),
                        "line": s.line_range.start(),
                        "out_degree": deg,
                    })
                })
                .collect();

            // Telemetry block — session uptime + lifetime cumulative.
            let agg = self.telemetry.aggregate();
            let uptime = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .saturating_sub(self.telemetry.session_started_at);
            let telemetry_block = serde_json::json!({
                "session_uptime_seconds": uptime,
                "calls": agg.calls,
                "response_tokens": agg.response_tokens,
                "baseline_tokens_avoided": agg
                    .static_baseline_tokens
                    .saturating_add(agg.measured_baseline_tokens),
                "tokens_saved": agg.tokens_saved(),
            });

            redact_response(
                serde_json::to_string(&serde_json::json!({
                    "files_indexed": file_count,
                    "total_symbols": symbol_count,
                    "tantivy_docs": tantivy_docs,
                    "schema_version": schema_version,
                    "repo_root": self.repo_root.to_string_lossy(),
                    "top_in_degree": top_in_degree,
                    "top_out_degree": top_out_degree,
                    "telemetry": telemetry_block,
                }))
                .unwrap_or_else(|e| format!("Error: {e}")),
            )
        })
        .await
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

/// Classification used by `code_find_strings` to filter hits between
/// `literal` (inside a string) and `comment` (after a comment marker).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StringHitKind {
    Literal,
    Comment,
    Neither,
}

impl StringHitKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Literal => "literal",
            Self::Comment => "comment",
            Self::Neither => "neither",
        }
    }
}

/// Per-line classification of a hit by lexical context. Heuristic — not a
/// full lexer — but distinguishes `// foo`, `/// foo`, `# foo`, `-- foo`,
/// and ` * foo` from string literals on the same line. For inline comments
/// (`fn x() // note`) we balance double-quote count before `//` so a `//`
/// inside a string isn't mistaken for a comment opener.
fn classify_string_hit(line: &str, pattern: &str) -> StringHitKind {
    let Some(match_idx) = line.find(pattern) else {
        return StringHitKind::Neither;
    };
    let prefix = &line[..match_idx];
    let trimmed = prefix.trim_start();

    // Whole-line comment markers: //, ///, /// , /*, *, #, --
    if trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with("--")
    {
        return StringHitKind::Comment;
    }
    // `#` is a comment in Python/Bash/Ruby/TOML but `#[derive(...)]` in Rust
    // is an attribute — exclude `#[`.
    if trimmed.starts_with('#') && !trimmed.starts_with("#[") && !trimmed.starts_with("#!") {
        return StringHitKind::Comment;
    }

    // Inline `//` after code: only counts as comment if it isn't inside a string.
    if let Some(slash_idx) = prefix.find("//") {
        let dq = prefix[..slash_idx].chars().filter(|&c| c == '"').count();
        if dq % 2 == 0 {
            return StringHitKind::Comment;
        }
    }

    // Literal detection: odd quote count before the match means we're inside one.
    let dq = prefix.chars().filter(|&c| c == '"').count();
    let sq = prefix.chars().filter(|&c| c == '\'').count();
    let bq = prefix.chars().filter(|&c| c == '`').count();
    if dq % 2 == 1 || sq % 2 == 1 || bq % 2 == 1 {
        return StringHitKind::Literal;
    }

    StringHitKind::Neither
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
    async fn code_outline_nests_impl_methods_under_struct() {
        // Regression: methods inside `impl Foo` were dropped from the outline
        // because the parser parented them to a Some(0) sentinel and the outline
        // filtered to parent_id.is_none(). Fix nests them under their type.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs_write(
            root.join("src/lib.rs"),
            "pub struct Greeter { name: String }\n\nimpl Greeter {\n    pub fn new(name: String) -> Self { Self { name } }\n    pub fn greet(&self) -> String { format!(\"hi {}\", self.name) }\n}\n\npub fn unrelated() {}\n",
        );

        let db_path = root.join(".recon").join("recon.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let store = Store::open(&db_path).unwrap();
        let tantivy_dir = root.join(".recon").join("tantivy");
        std::fs::create_dir_all(&tantivy_dir).unwrap();
        let tantivy = TantivyBackend::open(&tantivy_dir).unwrap();
        let server = ReconServer::new(root.to_path_buf(), store, tantivy).unwrap();
        server.index_repo().await.unwrap();

        use rmcp::handler::server::wrapper::Parameters;
        let params = Parameters(crate::tools::OutlineParams {
            path: "src/lib.rs".into(),
        });
        let result = server.code_outline(params).await;
        assert!(
            !result.starts_with("Error:"),
            "code_outline failed: {result}"
        );
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        let entries = json["entries"].as_array().expect("entries array");

        let greeter = entries
            .iter()
            .find(|e| e["name"] == "Greeter")
            .expect("Greeter struct must appear at top level");
        let children = greeter["children"]
            .as_array()
            .expect("Greeter must carry children");
        let child_names: Vec<&str> = children.iter().filter_map(|c| c["name"].as_str()).collect();
        assert!(
            child_names.contains(&"new"),
            "new method must nest under Greeter (got: {child_names:?})"
        );
        assert!(
            child_names.contains(&"greet"),
            "greet method must nest under Greeter (got: {child_names:?})"
        );

        // Methods must NOT appear as standalone top-level entries.
        let top_names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
        assert!(
            !top_names.contains(&"new"),
            "new method must not appear at top level (got: {top_names:?})"
        );
        assert!(
            !top_names.contains(&"greet"),
            "greet method must not appear at top level (got: {top_names:?})"
        );
        assert!(
            top_names.contains(&"unrelated"),
            "unrelated free function must appear at top level (got: {top_names:?})"
        );
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
        // v0.2.2 token diet: parent_chain is omitted from JSON when empty,
        // so for a top-level symbol the field should be either absent OR a
        // non-empty array. Either is a valid "we computed this" signal.
        match json.get("parent_chain") {
            None => {} // empty → omitted, fine
            Some(serde_json::Value::Array(arr)) => assert!(
                !arr.is_empty(),
                "parent_chain present but empty — should have been omitted"
            ),
            Some(other) => panic!("parent_chain has unexpected JSON type: {other}"),
        }
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
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("Hits"));
        assert_eq!(json["kind"].as_str(), Some("symbol"));
        let entries = json["hits"].as_array().expect("hits array");
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
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("Hits"));
        assert_eq!(json["kind"].as_str(), Some("symbol"));
        let entries = json["hits"].as_array().expect("hits array");
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
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("Hits"));
        assert_eq!(json["kind"].as_str(), Some("text"));
        let entries = json["hits"].as_array().expect("hits array");
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
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("Hits"));
        assert_eq!(json["kind"].as_str(), Some("file"));
        let entries = json["hits"].as_array().expect("hits array");
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
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("Hits"));
        assert_eq!(json["kind"].as_str(), Some("multi_find"));
        let entries = json["hits"].as_array().expect("hits array");
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

    // ── License expiry gate ────────────────────────────────────────────────
    // These tests pin the honor-until-period-end contract at the CLI boundary:
    // once a license's expires_at has passed, every tool call returns the
    // LicenseExpired error shape instead of running the tool. Renewal (via
    // `recon login`) refreshes the cache, the re-validation task swaps a
    // fresh license in, and tool calls resume.

    fn expired_license() -> crate::license::ValidatedLicense {
        crate::license::ValidatedLicense {
            tier: crate::router::Tier::new("Pro", crate::router::TierLimits::PRO),
            expires_at: 1_000_000_000, // 2001 — long past
            source: crate::license::LicenseSource::Cache,
            message: "Pro plan active until 2001".into(),
            revoked: false,
        }
    }

    fn fresh_license() -> crate::license::ValidatedLicense {
        crate::license::ValidatedLicense {
            tier: crate::router::Tier::new("Pro", crate::router::TierLimits::PRO),
            expires_at: u64::MAX / 2, // far future
            source: crate::license::LicenseSource::Cache,
            message: "Pro plan active".into(),
            revoked: false,
        }
    }

    fn revoked_license() -> crate::license::ValidatedLicense {
        crate::license::ValidatedLicense {
            tier: crate::router::Tier::new("Pro", crate::router::TierLimits::PRO),
            expires_at: u64::MAX / 2, // still in the future — revoke pre-empts
            source: crate::license::LicenseSource::Cache,
            message: "License revoked: worker rejected key".into(),
            revoked: true,
        }
    }

    #[tokio::test]
    async fn query_tool_gate_blocks_on_expired_license() {
        let (server, _tmp) = make_indexed_server().await;
        server.set_license(expired_license());

        let result = server.query_tool("code_stats", "{}").await;
        let err: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(err["shape"], "Error");
        assert_eq!(err["code"], ReconErrorCode::LicenseExpired.code());
        assert_eq!(err["kind"], "license_expired");
        assert!(err["message"].as_str().unwrap().contains("expired"));
        assert!(err["message"].as_str().unwrap().contains("recon login"));
        assert_eq!(err["data"]["tier"], "Pro");
    }

    #[tokio::test]
    async fn query_tool_gate_passes_with_fresh_license() {
        let (server, _tmp) = make_indexed_server().await;
        server.set_license(fresh_license());

        // With a fresh license the gate is silent; code_stats should succeed.
        let result = server.query_tool("code_stats", "{}").await;
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_ne!(
            v["shape"], "Error",
            "fresh license must not trigger the gate: {result}"
        );
    }

    #[tokio::test]
    async fn query_tool_gate_passes_when_no_license_installed() {
        // Library callers / tests that don't call set_license should still work
        // — the gate is opt-in via set_license so we don't break existing
        // test suites that construct ReconServer directly.
        let (server, _tmp) = make_indexed_server().await;
        let result = server.query_tool("code_stats", "{}").await;
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_ne!(v["shape"], "Error");
    }

    #[tokio::test]
    async fn query_tool_gate_swaps_license_atomically() {
        let (server, _tmp) = make_indexed_server().await;
        server.set_license(expired_license());

        // Expired → blocked.
        let blocked = server.query_tool("code_stats", "{}").await;
        let v: serde_json::Value = serde_json::from_str(&blocked).unwrap();
        assert_eq!(v["code"], ReconErrorCode::LicenseExpired.code());

        // Simulate the periodic re-validation task dropping in a fresh license.
        server.set_license(fresh_license());

        // After swap → unblocked on the next call.
        let ok = server.query_tool("code_stats", "{}").await;
        let v: serde_json::Value = serde_json::from_str(&ok).unwrap();
        assert_ne!(v["shape"], "Error");
    }

    #[tokio::test]
    async fn query_tool_gate_blocks_on_revoked_license() {
        // The revoke flow: expires_at is still in the future (user paid through
        // current period) but the worker has told us the key itself is dead.
        // The gate must fire regardless of calendar time.
        let (server, _tmp) = make_indexed_server().await;
        server.set_license(revoked_license());

        let result = server.query_tool("code_stats", "{}").await;
        let err: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(err["shape"], "Error");
        assert_eq!(err["code"], ReconErrorCode::LicenseExpired.code());
        assert_eq!(err["kind"], "license_expired");
        assert!(err["message"].as_str().unwrap().contains("recon login"));
    }

    #[test]
    fn is_expired_true_when_revoked_flag_set() {
        let mut lic = fresh_license();
        assert!(!lic.is_expired(), "fresh license must not look expired");
        lic.revoked = true;
        assert!(
            lic.is_expired(),
            "revoked flag must trigger is_expired regardless of expires_at"
        );
    }

    #[test]
    fn credentials_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert!(
            crate::license::read_credentials(dir).is_none(),
            "empty directory must report no credentials"
        );

        crate::license::save_credentials(dir, "sk-recon-roundtrip")
            .expect("save_credentials failed");
        let got = crate::license::read_credentials(dir).expect("read after save must succeed");
        assert_eq!(got, "sk-recon-roundtrip");

        // chmod 0600 only meaningful on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = crate::license::credentials_path(dir);
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "credentials file must be chmod 0600 on Unix");
        }

        crate::license::delete_credentials(dir).unwrap();
        assert!(
            crate::license::read_credentials(dir).is_none(),
            "after delete the file must be gone"
        );

        // Delete is idempotent — a second call on a missing file is not an error.
        crate::license::delete_credentials(dir).unwrap();
    }

    // ----------------------------------------------------------------
    // Phase 1+2 graph-traversal tool tests. Use a type-graph fixture
    // (struct-field type refs) which the parser walks reliably across
    // all languages — guaranteeing edges in the cached call graph.
    // ----------------------------------------------------------------

    async fn make_graph_fixture() -> (ReconServer, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs_write(
            root.join("src/lib.rs"),
            "pub mod auth;\npub mod session;\npub mod handler;\n",
        );
        fs_write(
            root.join("src/auth.rs"),
            "pub struct Token { pub value: u64 }\npub struct User { pub id: u64 }\n",
        );
        fs_write(
            root.join("src/session.rs"),
            "pub struct Session { pub user: crate::auth::User, pub start: u64 }\n",
        );
        fs_write(
            root.join("src/handler.rs"),
            "pub struct Handler { pub token: crate::auth::Token, pub session: crate::session::Session }\n",
        );
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

    #[tokio::test]
    async fn graph_fixture_has_refs() {
        // Sanity: confirm the parser+storage pipeline produces edges
        // for the graph fixture. Fails first if the parity work
        // regresses.
        let (server, _tmp) = make_graph_fixture().await;
        let refs = server.cached_all_refs();
        assert!(
            refs.len() > 4,
            "graph fixture should produce >4 refs, got {}",
            refs.len()
        );
    }

    #[tokio::test]
    async fn code_path_handler_to_user_via_session() {
        let (server, _tmp) = make_graph_fixture().await;
        use rmcp::handler::server::wrapper::Parameters;
        let result = server
            .code_path(Parameters(crate::tools::PathParams {
                src: "Handler".into(),
                dst: "User".into(),
                max_hops: 8,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            json["shape"].as_str(),
            Some("ReferenceDigest"),
            "expected ReferenceDigest, got: {result}"
        );
        let path = json["path"].as_array().expect("path field present");
        let qnames: Vec<&str> = path
            .iter()
            .filter_map(|h| h["qualified_name"].as_str())
            .collect();
        assert_eq!(qnames.first().copied(), Some("Handler"));
        assert_eq!(qnames.last().copied(), Some("User"));
        assert!(qnames.contains(&"Session"));
    }

    #[tokio::test]
    async fn code_path_unreachable_is_error() {
        let (server, _tmp) = make_graph_fixture().await;
        use rmcp::handler::server::wrapper::Parameters;
        let result = server
            .code_path(Parameters(crate::tools::PathParams {
                src: "User".into(),
                dst: "Handler".into(),
                max_hops: 8,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("Error"));
        assert_eq!(json["kind"].as_str(), Some("not_found"));
        assert_eq!(json["message"].as_str(), Some("unreachable"));
    }

    #[tokio::test]
    async fn code_path_rejects_max_hops_zero() {
        let (server, _tmp) = make_graph_fixture().await;
        use rmcp::handler::server::wrapper::Parameters;
        let result = server
            .code_path(Parameters(crate::tools::PathParams {
                src: "Handler".into(),
                dst: "User".into(),
                max_hops: 0,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("Error"));
        assert_eq!(json["kind"].as_str(), Some("invalid_params"));
    }

    #[tokio::test]
    async fn code_callers_finds_handler_uses_token() {
        let (server, _tmp) = make_graph_fixture().await;
        use rmcp::handler::server::wrapper::Parameters;
        let result = server
            .code_callers(Parameters(crate::tools::CallersParams {
                symbol: "Token".into(),
                depth: 1,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("ReferenceDigest"));
        let tiers = json["tiers"].as_array().expect("tiers array");
        let qnames: Vec<&str> = tiers
            .iter()
            .flat_map(|t| {
                t["refs"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|r| r["qualified_name"].as_str())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .collect();
        assert!(
            qnames.contains(&"Handler"),
            "expected Handler in Token's callers, got: {qnames:?}"
        );
    }

    #[tokio::test]
    async fn code_callees_layered_depth_2() {
        let (server, _tmp) = make_graph_fixture().await;
        use rmcp::handler::server::wrapper::Parameters;
        let result = server
            .code_callees(Parameters(crate::tools::CallersParams {
                symbol: "Handler".into(),
                depth: 2,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        let tiers = json["tiers"].as_array().expect("tiers array");
        let qnames: Vec<&str> = tiers
            .iter()
            .flat_map(|t| {
                t["refs"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|r| r["qualified_name"].as_str())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .collect();
        // Through Session, depth-2 should reach User.
        assert!(
            qnames.contains(&"User"),
            "depth-2 callees of Handler should reach User: {qnames:?}"
        );
    }

    #[tokio::test]
    async fn code_callers_rejects_depth_zero() {
        let (server, _tmp) = make_graph_fixture().await;
        use rmcp::handler::server::wrapper::Parameters;
        let result = server
            .code_callers(Parameters(crate::tools::CallersParams {
                symbol: "Token".into(),
                depth: 0,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("Error"));
        assert_eq!(json["kind"].as_str(), Some("invalid_params"));
    }

    #[tokio::test]
    async fn code_context_returns_envelope() {
        let (server, _tmp) = make_graph_fixture().await;
        use rmcp::handler::server::wrapper::Parameters;
        let result = server
            .code_context(Parameters(crate::tools::ContextParams {
                symbol: "Handler".into(),
                token_budget: 2000,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("SymbolCard"));
        assert_eq!(json["qualified_name"].as_str(), Some("Handler"));
        let context = json
            .get("context")
            .expect("context envelope must be present");
        let types = context["types"].as_array().cloned().unwrap_or_default();
        let type_qnames: Vec<&str> = types
            .iter()
            .filter_map(|c| c["qualified_name"].as_str())
            .collect();
        assert!(
            type_qnames.contains(&"Token") || type_qnames.contains(&"Session"),
            "expected Token or Session in Handler's referenced types: {type_qnames:?}"
        );
    }

    #[tokio::test]
    async fn code_context_unknown_symbol_is_not_found() {
        let (server, _tmp) = make_graph_fixture().await;
        use rmcp::handler::server::wrapper::Parameters;
        let result = server
            .code_context(Parameters(crate::tools::ContextParams {
                symbol: "no_such_function_anywhere".into(),
                token_budget: 2000,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("Error"));
        assert_eq!(json["kind"].as_str(), Some("not_found"));
    }

    #[tokio::test]
    async fn code_impact_reports_callers() {
        let (server, _tmp) = make_graph_fixture().await;
        use rmcp::handler::server::wrapper::Parameters;
        let result = server
            .code_impact(Parameters(crate::tools::ImpactParams {
                symbol: "User".into(),
                depth: 4,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("ReferenceDigest"));
        let tiers = json["tiers"].as_array().unwrap_or(&Vec::new()).clone();
        let qnames: Vec<&str> = tiers
            .iter()
            .flat_map(|t| {
                t["refs"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|r| r["qualified_name"].as_str())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .collect();
        assert!(
            qnames.contains(&"Session") || qnames.contains(&"Handler"),
            "expected Session or Handler in impact: {qnames:?}"
        );
    }

    #[tokio::test]
    async fn code_subsystems_lists_components() {
        let (server, _tmp) = make_graph_fixture().await;
        use rmcp::handler::server::wrapper::Parameters;
        let result = server
            .code_subsystems(Parameters(crate::tools::SubsystemsParams { limit: 50 }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("Skeleton"));
        let content = json["content"].as_str().expect("content").to_string();
        assert!(content.contains("# subsystems"));
        let body_lines = content.lines().filter(|l| !l.starts_with('#')).count();
        assert!(body_lines > 0, "no subsystem rows: {content}");
    }

    #[tokio::test]
    async fn code_subsystem_unknown_id_is_not_found() {
        let (server, _tmp) = make_graph_fixture().await;
        use rmcp::handler::server::wrapper::Parameters;
        let result = server
            .code_subsystem(Parameters(crate::tools::SubsystemParams {
                id: 99_999,
                token_budget: 1500,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("Error"));
        assert_eq!(json["kind"].as_str(), Some("not_found"));
    }

    #[tokio::test]
    async fn code_savings_reports_per_tool_breakdown() {
        let (server, _tmp) = make_graph_fixture().await;
        use rmcp::handler::server::wrapper::Parameters;
        // Run a known tool a few times to populate counters.
        for _ in 0..3 {
            let _ = server
                .code_outline(Parameters(crate::tools::OutlineParams {
                    path: "src/auth.rs".into(),
                }))
                .await;
        }
        let result = server
            .code_savings(Parameters(crate::tools::SavingsParams {}))
            .await;
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["shape"].as_str(), Some("Skeleton"));
        let content = json["content"].as_str().expect("content");
        assert!(
            content.contains("code_outline"),
            "code_savings should list code_outline after 3 calls: {content}"
        );
        // Aggregate trailer must be present.
        assert!(
            content.contains("# total"),
            "missing aggregate trailer: {content}"
        );
    }

    #[tokio::test]
    async fn code_stats_includes_telemetry_block() {
        let (server, _tmp) = make_graph_fixture().await;
        use rmcp::handler::server::wrapper::Parameters;
        // Trigger a call to populate counters.
        let _ = server
            .code_outline(Parameters(crate::tools::OutlineParams {
                path: "src/auth.rs".into(),
            }))
            .await;
        let result = server
            .code_stats(Parameters(crate::tools::StatsParams {}))
            .await;
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        let tel = json
            .get("telemetry")
            .expect("telemetry block must be present in code_stats");
        assert!(
            tel["calls"].as_u64().unwrap_or(0) > 0,
            "telemetry.calls should be > 0 after at least one tool call: {tel}"
        );
        assert!(json["top_in_degree"].is_array());
        assert!(json["top_out_degree"].is_array());
    }

    // ──── Shutdown-request notification ────
    //
    // The periodic license-revalidation task fires `request_shutdown()`
    // when the worker rejects the key. The serve loops `select!` on
    // `await_shutdown_request()`. These two need to compose: a
    // `request_shutdown()` call in one task must wake any
    // `await_shutdown_request()` in another within bounded latency.

    #[tokio::test]
    async fn await_shutdown_request_returns_after_request_shutdown() {
        let server = make_test_server();
        let s2 = server.clone();
        let waiter = tokio::spawn(async move { s2.await_shutdown_request().await });
        // Yield once so the waiter actually parks on `notified()`.
        tokio::task::yield_now().await;
        server.request_shutdown();
        // Bounded await — without the notify wakeup the test would hang.
        let res = tokio::time::timeout(std::time::Duration::from_secs(2), waiter).await;
        assert!(
            res.is_ok(),
            "request_shutdown must wake await_shutdown_request"
        );
        assert!(res.unwrap().is_ok());
    }

    #[tokio::test]
    async fn await_shutdown_request_short_circuits_after_request() {
        // Calling `request_shutdown()` BEFORE `await_shutdown_request()` must
        // still resolve immediately — `Notify` without permits would wait
        // forever, which is why the implementation checks the flag first.
        let server = make_test_server();
        server.request_shutdown();
        let res = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            server.await_shutdown_request(),
        )
        .await;
        assert!(
            res.is_ok(),
            "await_shutdown_request must short-circuit when flag already set"
        );
    }

    #[tokio::test]
    async fn request_shutdown_is_idempotent() {
        let server = make_test_server();
        // Three calls in a row must not panic, must leave the flag set,
        // and must not over-consume notify permits (Notify::notify_waiters
        // is permit-free, so this is mostly a guard against future regressions
        // if someone swaps to notify_one).
        server.request_shutdown();
        server.request_shutdown();
        server.request_shutdown();
        let res = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            server.await_shutdown_request(),
        )
        .await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn shutdown_method_also_wakes_waiters() {
        // The full `shutdown()` (final teardown) should also notify any
        // outstanding awaiters, so a stuck waiter doesn't outlive the
        // server. This is the path the serve loop hits after detecting
        // a transport close.
        let server = make_test_server();
        let s2 = server.clone();
        let waiter = tokio::spawn(async move { s2.await_shutdown_request().await });
        tokio::task::yield_now().await;
        server.shutdown().await;
        let res = tokio::time::timeout(std::time::Duration::from_secs(2), waiter).await;
        assert!(res.is_ok(), "shutdown() must wake await_shutdown_request");
    }

    #[tokio::test]
    async fn telemetry_persists_across_server_restarts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs_write(root.join("src/lib.rs"), "pub fn touch() -> u64 { 42 }\n");
        let db_path = root.join(".recon").join("recon.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let tantivy_dir = root.join(".recon").join("tantivy");
        std::fs::create_dir_all(&tantivy_dir).unwrap();

        // Session 1: call code_outline, shutdown to flush telemetry.
        {
            let store = Store::open(&db_path).unwrap();
            let tantivy = TantivyBackend::open(&tantivy_dir).unwrap();
            let server = ReconServer::new(root.to_path_buf(), store, tantivy).unwrap();
            server.index_repo().await.unwrap();
            use rmcp::handler::server::wrapper::Parameters;
            for _ in 0..5 {
                let _ = server
                    .code_outline(Parameters(crate::tools::OutlineParams {
                        path: "src/lib.rs".into(),
                    }))
                    .await;
            }
            server.shutdown().await;
        }

        // Session 2: re-open and verify lifetime counters survived.
        let store = Store::open(&db_path).unwrap();
        let tantivy = TantivyBackend::open(&tantivy_dir).unwrap();
        let server = ReconServer::new(root.to_path_buf(), store, tantivy).unwrap();
        let agg = server.telemetry.aggregate();
        assert!(
            agg.calls >= 5,
            "lifetime calls should survive restart, got {}",
            agg.calls
        );
    }

    // ── Step 8: end-to-end measured baselines per migrated tool ────────
    //
    // For each bucket-1 handler that ships with a per-call measurement,
    // exercise it once on a populated indexed repo and assert that:
    //   • the call accrued to `measured_baseline_tokens` (never zero on
    //     reasonable inputs);
    //   • the static counter for that tool stayed at 0 (the BASELINES
    //     entry is zeroed for migrated tools);
    //   • the MCP response shape is unchanged from the un-measured world.

    /// Look up a tool's CounterSnapshot by name from a Telemetry instance.
    /// Helper for the per-tool measured assertions below.
    fn snapshot_for(
        tel: &crate::telemetry::Telemetry,
        name: &str,
    ) -> crate::telemetry::CounterSnapshot {
        tel.per_tool_snapshots()
            .into_iter()
            .find(|(n, _)| *n == name)
            .map(|(_, s)| s)
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn measured_baseline_caches_by_path_and_mtime() {
        let (server, _tmp) = make_indexed_server().await;
        let abs = server.repo_root().join("src/math.rs");
        // First call populates the cache.
        let first = server.measure_read_baseline(&abs).await.unwrap();
        assert!(first > 0);
        assert_eq!(server.measured_baseline_cache.len(), 1);
        let cached = server
            .measured_baseline_cache
            .get(&abs)
            .map(|e| *e)
            .unwrap();
        // Second call hits the cache — same answer, no new entries.
        let second = server.measure_read_baseline(&abs).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(server.measured_baseline_cache.len(), 1);
        // Forge a stale mtime in the cache; next call must invalidate
        // and re-read (returning the same value computed from real bytes).
        server
            .measured_baseline_cache
            .insert(abs.clone(), (cached.0 - 1, cached.1 + 999));
        let third = server.measure_read_baseline(&abs).await.unwrap();
        assert_eq!(
            first, third,
            "stale-mtime entry must be replaced, not served"
        );
    }

    #[tokio::test]
    async fn measured_outline_credits_measured_baseline() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let _ = server
            .code_outline(Parameters(crate::tools::OutlineParams {
                path: "src/math.rs".into(),
            }))
            .await;
        let s = snapshot_for(&server.telemetry, "code_outline");
        assert_eq!(s.calls, 1);
        assert!(
            s.measured_baseline_tokens > 0,
            "code_outline must accrue measured baseline (got {s:?})"
        );
        assert_eq!(s.static_baseline_tokens, 0, "migrated tool: static stays 0");
    }

    #[tokio::test]
    async fn measured_skeleton_credits_measured_baseline() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let _ = server
            .code_skeleton(Parameters(crate::tools::SkeletonParams {
                path: "src/math.rs".into(),
                depth: 1,
            }))
            .await;
        let s = snapshot_for(&server.telemetry, "code_skeleton");
        assert_eq!(s.calls, 1);
        assert!(s.measured_baseline_tokens > 0);
        assert_eq!(s.static_baseline_tokens, 0);
    }

    #[tokio::test]
    async fn measured_context_credits_measured_baseline() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let _ = server
            .code_context(Parameters(crate::tools::ContextParams {
                symbol: "add".into(),
                token_budget: 2000,
            }))
            .await;
        let s = snapshot_for(&server.telemetry, "code_context");
        assert_eq!(s.calls, 1);
        assert!(
            s.measured_baseline_tokens > 0,
            "code_context must accrue measured baseline (got {s:?})"
        );
        assert_eq!(s.static_baseline_tokens, 0);
    }

    #[tokio::test]
    async fn measured_read_symbol_credits_measured_baseline() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let _ = server
            .code_read_symbol(Parameters(crate::tools::ReadSymbolParams {
                path: "src/math.rs".into(),
                symbol_or_line: "add".into(),
            }))
            .await;
        let s = snapshot_for(&server.telemetry, "code_read_symbol");
        assert_eq!(s.calls, 1);
        assert!(s.measured_baseline_tokens > 0);
        assert_eq!(s.static_baseline_tokens, 0);
    }

    #[tokio::test]
    async fn measured_search_exact_via_tantivy_credits_measured_baseline() {
        // Regression: in v0.4.0, the exact-mode Tantivy path passed
        // `None` to the telemetry recorder, so the most common
        // `code_search` call accrued zero savings. v0.4.1 sums
        // top-2 hit-file content tokens as the agent's grep+read
        // alternative.
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let _ = server
            .code_search(Parameters(crate::tools::SearchParams {
                query: "add".into(),
                mode: "exact".into(),
                filter: None,
            }))
            .await;
        let s = snapshot_for(&server.telemetry, "code_search");
        assert_eq!(s.calls, 1);
        assert!(
            s.measured_baseline_tokens > 0,
            "exact-mode Tantivy hit must accrue measured baseline (got {s:?})"
        );
        assert_eq!(s.static_baseline_tokens, 0);
    }

    #[tokio::test]
    async fn measured_search_regex_credits_measured_baseline() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        // regex mode forces the grep path (skips tantivy short-circuit),
        // so the measured baseline always reflects real match-line
        // tokens.
        let _ = server
            .code_search(Parameters(crate::tools::SearchParams {
                query: "fn \\w+".into(),
                mode: "regex".into(),
                filter: None,
            }))
            .await;
        let s = snapshot_for(&server.telemetry, "code_search");
        assert_eq!(s.calls, 1);
        assert!(
            s.measured_baseline_tokens > 0,
            "regex search must produce a measured baseline (got {s:?})"
        );
        assert_eq!(s.static_baseline_tokens, 0);
    }

    #[tokio::test]
    async fn measured_find_strings_credits_measured_baseline() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let _ = server
            .code_find_strings(Parameters(crate::tools::FindStringsParams {
                pattern: "Add".into(),
                kind: "both".into(),
                filter: None,
            }))
            .await;
        let s = snapshot_for(&server.telemetry, "code_find_strings");
        assert_eq!(s.calls, 1);
        assert!(s.measured_baseline_tokens > 0);
        assert_eq!(s.static_baseline_tokens, 0);
    }

    #[tokio::test]
    async fn measured_multi_find_credits_measured_baseline() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let _ = server
            .code_multi_find(Parameters(crate::tools::MultiFindParams {
                patterns: vec!["add".into(), "mul".into()],
                filter: None,
            }))
            .await;
        let s = snapshot_for(&server.telemetry, "code_multi_find");
        assert_eq!(s.calls, 1);
        assert!(
            s.measured_baseline_tokens > 0,
            "multi_find must accrue measured tokens across all patterns (got {s:?})"
        );
        assert_eq!(s.static_baseline_tokens, 0);
    }

    #[tokio::test]
    async fn measured_list_credits_measured_baseline() {
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let _ = server
            .code_list(Parameters(crate::tools::ListParams {
                lang: None,
                glob: None,
                filter: None,
            }))
            .await;
        let s = snapshot_for(&server.telemetry, "code_list");
        assert_eq!(s.calls, 1);
        assert!(s.measured_baseline_tokens > 0);
        assert_eq!(s.static_baseline_tokens, 0);
    }

    #[tokio::test]
    async fn static_only_tools_stay_on_static_baseline() {
        // The two index-driven tools that intentionally stay on the
        // static baseline (advisor: 3-tier ranking and ref-table lookup
        // have no clean grep equivalent). Invoke them and assert the
        // measured counter does NOT advance.
        let (server, _tmp) = make_indexed_server().await;
        use rmcp::handler::server::wrapper::Parameters;
        let _ = server
            .code_find_symbol(Parameters(crate::tools::FindSymbolParams {
                name: "add".into(),
                kind: None,
                lang: None,
            }))
            .await;
        let _ = server
            .code_find_refs(Parameters(crate::tools::FindRefsParams {
                symbol: "add".into(),
            }))
            .await;
        let fs = snapshot_for(&server.telemetry, "code_find_symbol");
        assert_eq!(
            fs.measured_baseline_tokens, 0,
            "code_find_symbol must remain on static-only baseline"
        );
        assert!(fs.static_baseline_tokens > 0);
        let fr = snapshot_for(&server.telemetry, "code_find_refs");
        assert_eq!(
            fr.measured_baseline_tokens, 0,
            "code_find_refs must remain on static-only baseline"
        );
        assert!(fr.static_baseline_tokens > 0);
    }
}
