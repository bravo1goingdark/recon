//! MCP server — fully wired: Tantivy search, PageRank, redaction, live watching.

use crate::tools::*;
use parking_lot::Mutex;
use rayon::prelude::*;
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
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, info, warn};

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
    /// Lock-free embedding service — shared via Arc, no mutex on hot path.
    #[cfg(feature = "embed")]
    embed_service: Arc<Mutex<Option<Arc<recon_embed::EmbedService>>>>,
    /// Lock-free read pool for vector similarity search.
    #[cfg(feature = "embed")]
    vec_read_pool: Arc<Mutex<Option<Arc<recon_embed::VecReadPool>>>>,
    /// Write handle — taken by `start_watcher`, None afterwards.
    #[cfg(feature = "embed")]
    vec_writer: Arc<Mutex<Option<recon_embed::VectorStore>>>,
}

fn redact_response(response: String) -> String {
    redact::redact_secrets(&response).unwrap_or(response)
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
            #[cfg(feature = "embed")]
            embed_service: Arc::new(Mutex::new(None)),
            #[cfg(feature = "embed")]
            vec_read_pool: Arc::new(Mutex::new(None)),
            #[cfg(feature = "embed")]
            vec_writer: Arc::new(Mutex::new(None)),
        })
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
        let embedder = recon_embed::Embedder::new()
            .map_err(|e| recon_core::error::Error::Search(format!("embed init: {e}")))?;
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
        info!(
            files = stats.files_indexed,
            symbols = stats.total_symbols,
            "initial indexing complete"
        );
        drop(tw);

        // Pre-warm the repo_map cache so the first user call is instant.
        // Runs PageRank in the background — doesn't block server startup.
        if stats.total_symbols > 0 {
            let all_symbols = self.read_pool.all_symbols().unwrap_or_default();
            let all_refs = self.read_pool.all_refs().unwrap_or_default();
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
        match tool_name {
            "code_outline" => match serde_json::from_str::<OutlineParams>(args_json) {
                Ok(p) => self.code_outline(Parameters(p)).await,
                Err(e) => format!("invalid args: {e}"),
            },
            "code_skeleton" => match serde_json::from_str::<SkeletonParams>(args_json) {
                Ok(p) => self.code_skeleton(Parameters(p)).await,
                Err(e) => format!("invalid args: {e}"),
            },
            "code_read_symbol" => match serde_json::from_str::<ReadSymbolParams>(args_json) {
                Ok(p) => self.code_read_symbol(Parameters(p)).await,
                Err(e) => format!("invalid args: {e}"),
            },
            "code_find_symbol" => match serde_json::from_str::<FindSymbolParams>(args_json) {
                Ok(p) => self.code_find_symbol(Parameters(p)).await,
                Err(e) => format!("invalid args: {e}"),
            },
            "code_find_refs" => match serde_json::from_str::<FindRefsParams>(args_json) {
                Ok(p) => self.code_find_refs(Parameters(p)).await,
                Err(e) => format!("invalid args: {e}"),
            },
            "code_search" => match serde_json::from_str::<SearchParams>(args_json) {
                Ok(p) => self.code_search(Parameters(p)).await,
                Err(e) => format!("invalid args: {e}"),
            },
            "code_list" => match serde_json::from_str::<ListParams>(args_json) {
                Ok(p) => self.code_list(Parameters(p)).await,
                Err(e) => format!("invalid args: {e}"),
            },
            "code_repo_map" => match serde_json::from_str::<RepoMapParams>(args_json) {
                Ok(p) => self.code_repo_map(Parameters(p)).await,
                Err(e) => format!("invalid args: {e}"),
            },
            "code_find_strings" => match serde_json::from_str::<FindStringsParams>(args_json) {
                Ok(p) => self.code_find_strings(Parameters(p)).await,
                Err(e) => format!("invalid args: {e}"),
            },
            "code_multi_find" => match serde_json::from_str::<MultiFindParams>(args_json) {
                Ok(p) => self.code_multi_find(Parameters(p)).await,
                Err(e) => format!("invalid args: {e}"),
            },
            "code_reindex" => match serde_json::from_str::<ReindexParams>(args_json) {
                Ok(p) => self.code_reindex(Parameters(p)).await,
                Err(e) => format!("invalid args: {e}"),
            },
            "code_stats" => match serde_json::from_str::<StatsParams>(args_json) {
                Ok(p) => self.code_stats(Parameters(p)).await,
                Err(e) => format!("invalid args: {e}"),
            },
            _ => format!("unknown tool: {tool_name}"),
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
        // Clone the Arc handles once; the hot path inside the loop needs no locks.
        #[cfg(feature = "embed")]
        let embed_svc: Option<Arc<recon_embed::EmbedService>> = self.embed_service.lock().clone();
        #[cfg(feature = "embed")]
        let vec_pool: Option<Arc<recon_embed::VecReadPool>> = self.vec_read_pool.lock().clone();
        // Take the write handle — watcher owns it exclusively from here.
        #[cfg(feature = "embed")]
        let vec_writer: Option<recon_embed::VectorStore> = self.vec_writer.lock().take();

        tokio::task::spawn_blocking(move || {
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
                            std::collections::HashMap::new()
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
                            let mut by_file: std::collections::HashMap<
                                &std::path::Path,
                                Vec<&recon_core::symbol::Symbol>,
                            > = std::collections::HashMap::new();
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

            while let Some(changed_paths) = watcher.recv() {
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
                        use std::collections::HashMap;
                        const EMBED_BATCH: usize = 64;

                        // relative-path → raw file bytes for symbol body extraction
                        let content_map: HashMap<std::path::PathBuf, &[u8]> = to_parse
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
                            let existing: HashMap<u64, [u8; 32]> =
                                pool.existing_hashes(&all_ids).unwrap_or_else(|e| {
                                    warn!("embed: existing_hashes: {e}");
                                    HashMap::new()
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
                }));

                if result.is_err() {
                    warn!("watcher batch panicked — recovering for next batch");
                }
            }
        });
    }

    fn resolve_path(&self, rel: &str) -> Result<PathBuf, String> {
        if redact::is_blocked_path(std::path::Path::new(rel)) {
            return Err(format!("access denied: sensitive file: {rel}"));
        }
        let path = self.repo_root.join(rel);
        let canonical = path
            .canonicalize()
            .map_err(|e| format!("path not found: {rel}: {e}"))?;
        // repo_root is already canonicalized at construction time
        if !canonical.starts_with(&self.repo_root) {
            return Err(format!("path traversal denied: {rel}"));
        }
        Ok(canonical)
    }

    /// Resolve indexed paths to absolute paths, applying an optional filter DSL.
    fn resolve_search_scope(&self, rel_paths: &[PathBuf], filter: Option<&str>) -> Vec<PathBuf> {
        let filtered = match filter {
            Some(f) if !f.is_empty() => match filters::parse_filter(f) {
                Ok(pf) => filters::apply_filter(rel_paths, &pf),
                Err(e) => {
                    warn!("filter parse error: {e}");
                    rel_paths.to_vec()
                }
            },
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
        if let Err(e) = self.resolve_path(&params.0.path) {
            return format!("Error: {e}");
        }
        let symbols = {
            let rel_path = PathBuf::from(&params.0.path);
            match self.read_pool.symbols_for_path(&rel_path) {
                Ok(s) => s,
                Err(e) => return format!("Error: {e}"),
            }
        };

        // O(n) child lookup: build parent_id -> children map in one pass
        let mut children_map: HashMap<u64, Vec<&recon_core::symbol::Symbol>> = HashMap::new();
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
        let abs_path = match self.resolve_path(&params.0.path) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };
        let content = match tokio::fs::read_to_string(&abs_path).await {
            Ok(c) => c,
            Err(e) => return format!("Error reading file: {e}"),
        };

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
            Err(e) => return format!("Error: {e}"),
        };
        let content = match tokio::fs::read_to_string(&abs_path).await {
            Ok(c) => c,
            Err(e) => return format!("Error: {e}"),
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
            None => return format!("Symbol not found: {}", params.0.symbol_or_line),
        };

        let body = content
            .get(sym.byte_range.clone())
            .unwrap_or("[byte range out of bounds]")
            .to_string();

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

        let view = ToolOutput::SymbolCard(SymbolCardView {
            path: rel_path,
            qualified_name: sym.qualified_name.to_string(),
            kind: sym.kind,
            signature: sym.signature.as_deref().map(str::to_owned),
            doc: sym.doc.as_deref().map(str::to_owned),
            body,
            line_range: (*sym.line_range.start(), *sym.line_range.end()),
            parent_chain: vec![],
            callers,
            callees: vec![],
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

        let entries: Vec<serde_json::Value> = results
            .iter()
            .map(|s| {
                serde_json::json!({
                    "qualified_name": s.qualified_name.as_str(),
                    "path": s.path.to_string_lossy(),
                    "line": *s.line_range.start(),
                    "kind": s.kind.label(),
                    "signature": s.signature,
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

        let top_k: Vec<RefEntry> = refs
            .iter()
            .take(20)
            .map(|r| RefEntry {
                path: (*r.src_path).clone(),
                line: 0,
                col: None,
                snippet: r.ident.clone(),
                enclosing_symbol: None,
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
        let paths = self.read_pool.all_file_paths().unwrap_or_default();

        let abs_paths = self.resolve_search_scope(&paths, params.0.filter.as_deref());

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

        let mut entries: Vec<serde_json::Value> = Vec::with_capacity(summaries.len());
        for (path, sym_count, top_syms) in &summaries {
            if let Some(ref pf) = filter_parsed {
                if filters::apply_filter(std::slice::from_ref(path), pf).is_empty() {
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

        // All reads go through lock-free ReadPool
        let (all_symbols, all_refs, cache_key) = {
            // Check cache for unfocused maps
            if focus_files.is_empty() {
                let last_idx = self.read_pool.max_indexed_at().unwrap_or(0);
                let key = format!("map_cache:{}:{}", last_idx, budget);
                if let Ok(Some(cached)) = self.read_pool.get_meta(&key) {
                    return cached;
                }
                let syms = self.read_pool.all_symbols().unwrap_or_default();
                let refs = self.read_pool.all_refs().unwrap_or_default();
                (syms, refs, Some(key))
            } else {
                let syms = self.read_pool.all_symbols().unwrap_or_default();
                let refs = self.read_pool.all_refs().unwrap_or_default();
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
        let paths = self.read_pool.all_file_paths().unwrap_or_default();

        let abs_paths = self.resolve_search_scope(&paths, params.0.filter.as_deref());
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
        let paths = self.read_pool.all_file_paths().unwrap_or_default();

        let abs_paths = self.resolve_search_scope(&paths, params.0.filter.as_deref());
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
    async fn code_reindex(&self, _params: Parameters<ReindexParams>) -> String {
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

            // Phase 1: Walk + parse (NO locks held)
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

            // Phase 2: Batch store in 500-file chunks — lock released between chunks
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
                // Lock released here between chunks — reads can proceed
            }

            // Phase 3: Tantivy indexing (short lock)
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
            })
        })
        .await;

        match result {
            Ok(stats) => serde_json::to_string(&stats).unwrap_or_else(|e| format!("Error: {e}")),
            Err(e) => format!("Reindex failed: {e}"),
        }
    }

    #[tool(
        name = "code_stats",
        description = "Report index health: total files, symbols, last indexed time, Tantivy doc count. Use to check if the index is fresh and complete."
    )]
    async fn code_stats(&self, _params: Parameters<StatsParams>) -> String {
        let file_count = self
            .read_pool
            .all_file_paths()
            .map(|p| p.len())
            .unwrap_or(0);
        let symbol_count = self.read_pool.symbol_count().unwrap_or(0);
        let tantivy_docs = self.tantivy.doc_count();
        let schema_version = self
            .read_pool
            .get_meta("schema_version")
            .unwrap_or(None)
            .unwrap_or_default();

        serde_json::to_string(&serde_json::json!({
            "files_indexed": file_count,
            "total_symbols": symbol_count,
            "tantivy_docs": tantivy_docs,
            "schema_version": schema_version,
            "repo_root": self.repo_root.to_string_lossy(),
        }))
        .unwrap_or_else(|e| format!("Error: {e}"))
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

    #[test]
    fn server_new_does_not_panic() {
        // Regression: Server::new must never panic; errors should propagate.
        let _server = make_test_server();
    }

    #[test]
    fn server_new_returns_result() {
        // Verify the Result-returning API: Ok on a valid in-memory setup.
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
        // File not indexed — must return an error message, not panic.
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
        // Empty repo map is valid; must not panic.
        assert!(!result.is_empty());
    }
}
