//! Multi-repo MCP service.
//!
//! Wraps [`crate::router::RepoRouter`] and an active-repo `ArcSwap<ReconServer>`
//! and exposes two new MCP tools — `code_activate_repo` and
//! `code_list_repos` — alongside thin shims that delegate every existing
//! stateful tool to whichever [`crate::server::ReconServer`] is currently
//! active.
//!
//! ## Hot-path cost
//!
//! Every tool call performs one [`arc_swap::ArcSwap::load_full`] — a
//! relaxed atomic load plus an `Arc::clone` (≈3 ns) — and then forwards
//! to the active server. Same cost class as the single-repo `&self`
//! access today; the multi-repo layer is effectively free on the
//! steady-state path.
//!
//! ## Session persistence
//!
//! On every successful `code_activate_repo` (and on graceful shutdown)
//! the loaded-repo set and the active repo are written to
//! `<config_dir>/sessions.json` via `save_session`. On startup
//! [`MultiRepoService::restore_session`] re-loads each remembered repo
//! through the router (subject to the current tier's repo limit) so
//! agents do not have to re-issue `code_activate_repo` after every
//! `recon serve` restart.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::router::{RepoRouter, RouterError};
use crate::server::ReconServer;
use crate::tools::*;
use recon_core::shapes::{HitsView, SkeletonView, ToolOutput};

// ── Param types for the two new tools ────────────────────────────────────────

/// Parameters for `code_activate_repo`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ActivateRepoParams {
    /// Absolute or relative path to the repository root. Resolved via
    /// `Path::canonicalize` before being passed to the router.
    pub path: String,
}

/// Parameters for `code_list_repos`.
#[derive(Debug, Deserialize, Serialize, JsonSchema, Default)]
pub struct ListReposParams {}

// ── Session persistence ──────────────────────────────────────────────────────

/// Persisted multi-repo session — re-loaded on `recon serve` restart so
/// agents do not need to call `code_activate_repo` on every reconnect.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionState {
    /// Schema version; bumped only when the on-disk shape changes.
    pub version: u32,
    /// Currently-active repo path (absolute, canonicalized).
    pub active: String,
    /// All loaded repo paths (absolute, canonicalized). Includes `active`.
    pub loaded: Vec<String>,
}

fn session_path(config_dir: &Path) -> PathBuf {
    config_dir.join("sessions.json")
}

/// Read `<config_dir>/sessions.json`. Returns `Ok(None)` when absent.
pub fn load_session(config_dir: &Path) -> Result<Option<SessionState>, String> {
    let path = session_path(config_dir);
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let state: SessionState = serde_json::from_str(&content).map_err(|e| e.to_string())?;
    Ok(Some(state))
}

/// Persist `state` to `<config_dir>/sessions.json` atomically (tempfile + rename).
pub fn save_session(config_dir: &Path, state: &SessionState) -> Result<(), String> {
    std::fs::create_dir_all(config_dir).map_err(|e| e.to_string())?;
    let path = session_path(config_dir);
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

// ── MultiRepoService ─────────────────────────────────────────────────────────

/// Multi-repo MCP service. Holds the [`RepoRouter`] (which enforces tier
/// limits and owns each loaded [`ReconServer`]) plus an `ArcSwap` to the
/// currently-active server. Tool calls delegate to the active server via
/// a one-line shim per tool.
#[derive(Clone)]
pub struct MultiRepoService {
    #[allow(dead_code)] // read by the #[tool_router] macro expansion
    tool_router: ToolRouter<Self>,
    /// Multi-repo state. Stored under `Arc` so the service itself is
    /// cheap to clone for handler binding and so the router survives a
    /// service drop in `tokio::spawn`'d tasks.
    inner: Arc<MultiRepoInner>,
}

struct MultiRepoInner {
    router: Arc<RepoRouter>,
    /// The currently-active repo's server. Hot-path: `load_full()` is one
    /// relaxed atomic load + `Arc::clone` (≈3 ns).
    active: ArcSwap<ReconServer>,
    /// Where to read/write `sessions.json`. Typically
    /// `<config_dir>/recon/`.
    config_dir: PathBuf,
}

impl MultiRepoService {
    /// Construct a multi-repo service rooted at `initial` as the active
    /// repo. The router is shared so callers can hold their own handle
    /// (e.g. a periodic `sweep_expired` task).
    pub fn new(router: Arc<RepoRouter>, initial: ReconServer, config_dir: PathBuf) -> Self {
        let inner = Arc::new(MultiRepoInner {
            router,
            active: ArcSwap::new(Arc::new(initial)),
            config_dir,
        });
        Self {
            tool_router: Self::tool_router(),
            inner,
        }
    }

    /// The currently-active repo's server. Cheap atomic load; the
    /// returned `Arc<ReconServer>` is independent of subsequent swaps.
    pub fn active(&self) -> Arc<ReconServer> {
        self.inner.active.load_full()
    }

    /// Set `path` as the active repo, loading it through the router if
    /// not already loaded. Caller is responsible for any tier-limit
    /// handling — this method propagates [`RouterError`] verbatim.
    pub fn activate(&self, path: &Path) -> Result<Arc<ReconServer>, RouterError> {
        let server = self.inner.router.get_or_load(path)?;
        let arc = Arc::new(server);
        self.inner.active.store(arc.clone());
        Ok(arc)
    }

    /// Persist the current loaded set + active repo to
    /// `<config_dir>/sessions.json`. Best-effort — failures are warned,
    /// not propagated, since persistence is incidental to tool dispatch.
    pub fn persist_session(&self) {
        let active_path = self.inner.active.load_full().repo_root().to_path_buf();
        let loaded: Vec<String> = self
            .inner
            .router
            .loaded_repos()
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let state = SessionState {
            version: 1,
            active: active_path.to_string_lossy().into_owned(),
            loaded,
        };
        if let Err(e) = save_session(&self.inner.config_dir, &state) {
            warn!(%e, "failed to persist multi-repo session");
        }
    }

    /// Re-load every repo recorded in the previous session. Repos that
    /// fail to load (path moved, exceeds current tier, etc.) are warned
    /// and skipped — startup never blocks on a stale session.
    ///
    /// Returns the count of repos actually re-loaded.
    pub fn restore_session(&self) -> usize {
        let session = match load_session(&self.inner.config_dir) {
            Ok(Some(s)) => s,
            Ok(None) => return 0,
            Err(e) => {
                warn!(%e, "could not read sessions.json — skipping session restore");
                return 0;
            }
        };
        let mut loaded = 0usize;
        let initial = self.inner.active.load_full().repo_root().to_path_buf();
        for path_str in &session.loaded {
            let path = Path::new(path_str);
            if path == initial.as_path() {
                // Already loaded as the initial active repo.
                loaded += 1;
                continue;
            }
            match self.inner.router.get_or_load(path) {
                Ok(_) => {
                    loaded += 1;
                    info!(repo = %path.display(), "restored repo from session");
                }
                Err(e) => warn!(
                    repo = %path.display(),
                    error = %e,
                    "skipping repo from session — could not re-load"
                ),
            }
        }
        // If the saved active was successfully re-loaded and isn't the
        // current initial, swap it in.
        if !session.active.is_empty() && Path::new(&session.active) != initial {
            if let Ok(server) = self.inner.router.get_or_load(Path::new(&session.active)) {
                self.inner.active.store(Arc::new(server));
            }
        }
        loaded
    }
}

// ── Tool registrations ───────────────────────────────────────────────────────

#[allow(missing_docs)]
#[tool_router(router = tool_router)]
impl MultiRepoService {
    #[tool(
        name = "code_activate_repo",
        description = "Switch the active repository for subsequent stateful tool calls. Loads the repo into the router (subject to tier limits) and atomically swaps the active server pointer. Subsequent code_outline / code_find_symbol / etc. calls then operate on the newly-activated repo. Returns the activated repo's path plus its file and symbol counts as a Skeleton view. Errors with kind `tier_limit` when the current tier's repo limit would be exceeded; reissue after `code_list_repos` shows you which repo to swap out."
    )]
    async fn code_activate_repo(&self, p: Parameters<ActivateRepoParams>) -> String {
        let raw = p.0.path;
        let canonical = match Path::new(&raw).canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return serde_json::to_string(&ToolOutput::Skeleton(SkeletonView {
                    path: None,
                    content: format!("error: cannot canonicalize {raw:?}: {e}"),
                    token_estimate: 16,
                }))
                .unwrap_or_else(|e| format!("Error: {e}"));
            }
        };
        match self.activate(&canonical) {
            Ok(server) => {
                let files = server.file_count();
                let symbols = server.symbol_count();
                self.persist_session();
                let view = ToolOutput::Skeleton(SkeletonView {
                    path: Some(canonical.clone()),
                    content: format!(
                        "active repo: {}\nfiles: {files}\nsymbols: {symbols}",
                        canonical.display()
                    ),
                    token_estimate: 32,
                });
                serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}"))
            }
            Err(e) => router_error_response(&e),
        }
    }

    #[tool(
        name = "code_list_repos",
        description = "List every repository currently loaded in the multi-repo router. Each row carries `path`, `files`, `symbols`, and `active` (true for the currently-active repo). Use to discover what is loaded before calling `code_activate_repo` — especially under a tier with a repo limit."
    )]
    async fn code_list_repos(&self, _p: Parameters<ListReposParams>) -> String {
        let active_path = self.inner.active.load_full().repo_root().to_path_buf();
        let stats = self.inner.router.loaded_repos_with_stats();
        let entries: Vec<serde_json::Value> = stats
            .into_iter()
            .map(|(path, files, symbols)| {
                serde_json::json!({
                    "path": path.display().to_string(),
                    "files": files,
                    "symbols": symbols,
                    "active": path == active_path,
                })
            })
            .collect();
        let view = ToolOutput::Hits(HitsView {
            kind: "repo".into(),
            count: entries.len(),
            hits: entries,
            truncated: false,
        });
        serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}"))
    }

    // ── 18 thin shims for the existing stateful tools ────────────────────────
    // Each delegates to the active repo via one ArcSwap load + Arc::clone,
    // then calls into ReconServer's pub method. The `description` is
    // duplicated from server.rs verbatim because rmcp's #[tool] macro
    // requires a string-literal description per registration.

    #[tool(
        name = "code_outline",
        description = "Show one-line-per-symbol outline of a file. Returns symbol kinds, names, and line numbers in a tree structure. Use instead of Read when you need to understand a file's structure without reading its full content. Typical output: 300-500 tokens for a 500-line file."
    )]
    async fn code_outline(&self, p: Parameters<OutlineParams>) -> String {
        self.active().code_outline(p).await
    }

    #[tool(
        name = "code_skeleton",
        description = "Show signatures and docstrings with bodies elided as '...'. 10x compression vs full file read. Use instead of Read when you need to understand APIs and structure. Output: ~300 tokens per 3000-token file."
    )]
    async fn code_skeleton(&self, p: Parameters<SkeletonParams>) -> String {
        self.active().code_skeleton(p).await
    }

    #[tool(
        name = "code_read_symbol",
        description = "Read the full source of one symbol plus its parent chain and caller/callee references. Use instead of Read when you need one specific function or type. Output: ~200-800 tokens."
    )]
    async fn code_read_symbol(&self, p: Parameters<ReadSymbolParams>) -> String {
        self.active().code_read_symbol(p).await
    }

    #[tool(
        name = "code_find_symbol",
        description = "Find symbols by name across the codebase. Tiered: exact SQLite match -> Tantivy BM25 -> FTS5 trigram + nucleo fuzzy. Use instead of Grep when searching for functions, types, or classes."
    )]
    async fn code_find_symbol(&self, p: Parameters<FindSymbolParams>) -> String {
        self.active().code_find_symbol(p).await
    }

    #[tool(
        name = "code_find_refs",
        description = "Find all references to a symbol. Returns a count and top-k call sites as path:line triples. Use instead of Grep for finding usages of a function or type."
    )]
    async fn code_find_refs(&self, p: Parameters<FindRefsParams>) -> String {
        self.active().code_find_refs(p).await
    }

    #[tool(
        name = "code_path",
        description = "Shortest call-graph path from `src` to `dst`. Use to answer 'how does X reach Y?' — replaces a chain of code_find_refs calls. Both arguments accept a bare name or a fully qualified name (preferred — disambiguates). Returns an ordered hop sequence with file:line per hop. When unreachable within `max_hops` (default 8, max 16) returns an Error with kind 'not_found'/'unreachable' plus an `unresolved_hint` when the BFS hit a likely dyn-dispatch / FFI boundary. When src or dst is ambiguous (multiple symbols share the name) the BFS spans the cross-product and returns the shortest match. Bidirectional BFS over the cached reference graph; total-visit cap 50 000 nodes. Output uses ReferenceDigest with the `path` field populated."
    )]
    async fn code_path(&self, p: Parameters<PathParams>) -> String {
        self.active().code_path(p).await
    }

    #[tool(
        name = "code_callers",
        description = "Transitive callers of `symbol` up to `depth` rings (default 1, max 6). Replaces depth-many chained code_find_refs calls. Returns one tier per ring with the symbols at that depth. Cycle-safe (each symbol emitted at its minimum depth only). Per-tier fan-out is capped at 50 to bound god-node responses; total-visit cap 50 000 nodes. When either cap fires `truncated: true` is set. Returns symbol identities (qname + path + line of definition), not call-site lines — use code_find_refs for the lexical call-site digest. `symbol` accepts bare or fully qualified names; ambiguous bare names traverse from all matches. Output uses ReferenceDigest with the `tiers` field populated."
    )]
    async fn code_callers(&self, p: Parameters<CallersParams>) -> String {
        self.active().code_callers(p).await
    }

    #[tool(
        name = "code_callees",
        description = "Transitive callees of `symbol` up to `depth` rings (default 1, max 6). Mirror of code_callers — what does this symbol call (directly and transitively)? Cycle-safe, per-tier fan-out capped at 50, total-visit cap 50 000. `truncated: true` when caps fire. Returns symbol identities (qname + path + line of definition), not call-site lines. Use this to understand what changing X *requires* you to also understand (callees) versus what changing X *risks breaking* (callers). Output uses ReferenceDigest with the `tiers` field populated."
    )]
    async fn code_callees(&self, p: Parameters<CallersParams>) -> String {
        self.active().code_callees(p).await
    }

    #[tool(
        name = "code_context",
        description = "One-shot bundle of everything an agent needs to reason about a symbol — replaces the canonical 4-call understand-X loop (find_symbol → read_symbol → find_refs → search-for-tests). Returns: (1) the target symbol's signature + doc + first ~20 body lines, (2) up to 5 immediate callers, (3) up to 5 immediate callees, (4) up to 3 referenced types, (5) up to 3 tests that exercise it. Honors `token_budget` (default 2000); drops sections under pressure in this order: tests → callees → types → callers (skeleton+body always kept). Accepts a bare name or a fully qualified name. When ambiguous (multiple symbols share the bare name) returns an Error with kind 'invalid_params' listing up to 5 candidates; reissue with a qualified name. Output uses SymbolCard with the `context` envelope populated. Test detection in v0.3 is Rust-only (tests::* qname patterns and test_* / Test* function names); cross-language coverage is on the v0.4 roadmap."
    )]
    async fn code_context(&self, p: Parameters<ContextParams>) -> String {
        self.active().code_context(p).await
    }

    #[tool(
        name = "code_impact",
        description = "Blast radius of changing `symbol` — transitive callers up to `depth` rings (default 4, max 6) plus tests that exercise it. Returns one tier per ring (production callers), a separate `tests` array for transitively-reaching test functions (Rust-only Phase-1 detector: tests::* qnames + test_* / Test* names), and `truncated: true` when fan-out caps fire. Use to answer 'what might break if I change X?' before refactoring. Per-tier fan-out cap 50, total-visit cap 50 000 — a god-node query terminates with a marker rather than blowing up. Output uses ReferenceDigest with the `tiers` and `tests` fields populated."
    )]
    async fn code_impact(&self, p: Parameters<ImpactParams>) -> String {
        self.active().code_impact(p).await
    }

    #[tool(
        name = "code_subsystems",
        description = "List the natural subsystems of the repo — weakly-connected components of the reference graph. Each subsystem has an id (use with code_subsystem), the qualified-name of its highest-degree symbol (the 'hub'), the dominant directory, and a symbol count. Use to orient yourself before drilling in: subsystems separate cleanly along architectural lines (e.g. recon-search vs recon-storage) without you having to know the directory structure. Sorted by symbol count descending. `limit` caps the number returned (default 50). Output uses Skeleton with subsystems rendered as one line each. Phase 2 v0.3.x: connected components only. Future v0.4.x adds Leiden modularity-optimized clustering."
    )]
    async fn code_subsystems(&self, p: Parameters<SubsystemsParams>) -> String {
        self.active().code_subsystems(p).await
    }

    #[tool(
        name = "code_subsystem",
        description = "Detailed view of one subsystem (from code_subsystems). Returns a skeleton-style summary of all symbols in the component — qname, kind, file:line — within `token_budget` tokens (default 1500). Use after code_subsystems to drill into a specific cluster without reading every file in the directory. Output uses Skeleton."
    )]
    async fn code_subsystem(&self, p: Parameters<SubsystemParams>) -> String {
        self.active().code_subsystem(p).await
    }

    #[tool(
        name = "code_search",
        description = "Search for text patterns. Modes: exact (default), regex, hybrid (BM25 + text fused via reciprocal rank fusion). Use instead of Grep for code search."
    )]
    async fn code_search(&self, p: Parameters<SearchParams>) -> String {
        self.active().code_search(p).await
    }

    #[tool(
        name = "code_list",
        description = "List indexed source files with language, line count, and top symbols. Use instead of Glob when you need structured file listings. Supports language filter."
    )]
    async fn code_list(&self, p: Parameters<ListParams>) -> String {
        self.active().code_list(p).await
    }

    #[tool(
        name = "code_repo_map",
        description = "Generate a ranked overview of the most important symbols in the repo. Uses personalized PageRank over the reference graph with Aider-style edge weights. Output fits within a token budget (default 2000). Best first tool to call for orientation."
    )]
    async fn code_repo_map(&self, p: Parameters<RepoMapParams>) -> String {
        self.active().code_repo_map(p).await
    }

    #[tool(
        name = "code_find_strings",
        description = "Search for patterns in string literals and comments. Finds SQL fragments, i18n keys, log messages that structural search misses."
    )]
    async fn code_find_strings(&self, p: Parameters<FindStringsParams>) -> String {
        self.active().code_find_strings(p).await
    }

    #[tool(
        name = "code_multi_find",
        description = "Search for multiple patterns at once. More efficient than multiple code_search calls. Returns results grouped by pattern."
    )]
    async fn code_multi_find(&self, p: Parameters<MultiFindParams>) -> String {
        self.active().code_multi_find(p).await
    }

    #[tool(
        name = "code_reindex",
        description = "Trigger a full re-index of the repository. Use when you suspect the index is stale or after major file changes outside the editor."
    )]
    async fn code_reindex(&self, p: Parameters<ReindexParams>) -> String {
        self.active().code_reindex(p).await
    }
}

// ── ServerHandler — same instructions blob as ReconServer plus a pointer
//    to the multi-repo tools.

#[tool_handler]
impl ServerHandler for MultiRepoService {
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
             Multi-repo: call code_activate_repo to switch the active repo \
             for subsequent stateful tools, code_list_repos to discover \
             what's loaded. These tools return structured, token-efficient \
             results. Use Read only when you need the exact source of a \
             specific symbol (prefer code_read_symbol for that)."
                .to_string(),
        )
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn router_error_response(e: &RouterError) -> String {
    let view = ToolOutput::Skeleton(SkeletonView {
        path: None,
        content: format!("router error: {e}"),
        token_estimate: 16,
    });
    serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let original = SessionState {
            version: 1,
            active: "/home/u/repo-a".into(),
            loaded: vec!["/home/u/repo-a".into(), "/home/u/repo-b".into()],
        };
        save_session(dir.path(), &original).unwrap();
        let loaded = load_session(dir.path()).unwrap().expect("session exists");
        assert_eq!(loaded.active, original.active);
        assert_eq!(loaded.loaded, original.loaded);
    }

    #[test]
    fn load_session_returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_session(dir.path()).unwrap().is_none());
    }
}
