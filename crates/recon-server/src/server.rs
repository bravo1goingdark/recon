//! MCP server — fully wired: Tantivy search, PageRank, redaction, live watching.

use crate::tools::*;
use parking_lot::Mutex;
use recon_core::lang::Language;
use recon_core::redact;
use recon_core::shapes::*;
use recon_indexer::indexer;
use recon_indexer::watcher::Watcher;
use recon_search::fff_backend::FffBackend;
use recon_search::search_trait::{TextQuery, TextSearcher};
use recon_search::{filters, fuzzy, pagerank, tantivy_backend::TantivyBackend};
use recon_storage::store::Store;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};

/// The recon MCP server.
#[derive(Clone)]
pub struct ReconServer {
    #[allow(dead_code)] // read by the #[tool_router] macro expansion
    tool_router: ToolRouter<Self>,
    store: Arc<Mutex<Store>>,
    tantivy: Arc<TantivyBackend>,
    text_searcher: Arc<dyn TextSearcher>,
    repo_root: PathBuf,
    #[cfg(feature = "embed")]
    embedder: Arc<Mutex<Option<recon_embed::Embedder>>>,
    #[cfg(feature = "embed")]
    vector_store: Arc<Mutex<Option<recon_embed::VectorStore>>>,
}

fn redact_response(response: String) -> String {
    redact::redact_secrets(&response).unwrap_or(response)
}

impl ReconServer {
    /// Create a new server for the given repo root.
    pub fn new(repo_root: PathBuf, store: Store, tantivy: TantivyBackend) -> Self {
        Self {
            tool_router: Self::tool_router(),
            store: Arc::new(Mutex::new(store)),
            tantivy: Arc::new(tantivy),
            text_searcher: Arc::new(FffBackend::new()),
            repo_root,
            #[cfg(feature = "embed")]
            embedder: Arc::new(Mutex::new(None)),
            #[cfg(feature = "embed")]
            vector_store: Arc::new(Mutex::new(None)),
        }
    }

    /// Initialize the embedding engine (model download on first run).
    #[cfg(feature = "embed")]
    pub async fn init_embed(&self) -> Result<(), recon_core::error::Error> {
        let store_dir = self.repo_root.join(".recon");
        let embedder = recon_embed::Embedder::new()
            .map_err(|e| recon_core::error::Error::Search(format!("embed init: {e}")))?;
        let vs = recon_embed::VectorStore::open(&store_dir.join("vectors"))
            .await
            .map_err(|e| recon_core::error::Error::Search(format!("vector store: {e}")))?;
        *self.embedder.lock() = Some(embedder);
        *self.vector_store.lock() = Some(vs);
        info!("embedding engine initialized");
        Ok(())
    }

    /// Run initial indexing of the repo (SQLite + Tantivy).
    pub async fn index_repo(&self) -> Result<(), recon_core::error::Error> {
        let store = self.store.lock();
        let stats = indexer::index_repo_incremental(&store, Some(&self.tantivy), &self.repo_root)?;
        info!(
            files = stats.files_indexed,
            symbols = stats.total_symbols,
            "initial indexing complete"
        );
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
    pub fn start_watcher(&self) {
        let store = self.store.clone();
        let tantivy = self.tantivy.clone();
        let repo_root = self.repo_root.clone();

        tokio::spawn(async move {
            let watcher = match Watcher::new(&repo_root) {
                Ok(w) => w,
                Err(e) => {
                    warn!("failed to start watcher: {e}");
                    return;
                }
            };
            info!("file watcher started");

            while let Some(changed_paths) = watcher.recv() {
                let store = store.lock();
                let mut writer = match tantivy.writer(15_000_000) {
                    Ok(w) => w,
                    Err(e) => {
                        warn!("tantivy writer error: {e}");
                        continue;
                    }
                };

                for path in &changed_paths {
                    if let Err(e) = indexer::index_file(
                        &store,
                        Some(&tantivy),
                        Some(&mut writer),
                        path,
                        &repo_root,
                    ) {
                        warn!(?path, "re-index error: {e}");
                    }
                }

                if let Err(e) = tantivy.commit(&mut writer) {
                    warn!("tantivy commit error on watch: {e}");
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
        let store = self.store.lock();
        let rel_path = PathBuf::from(&params.0.path);
        let symbols = match store.symbols_for_path(&rel_path) {
            Ok(s) => s,
            Err(e) => return format!("Error: {e}"),
        };

        let mut entries = Vec::new();
        for sym in &symbols {
            if sym.parent_id.is_none() {
                entries.push(OutlineEntry {
                    kind: sym.kind,
                    name: sym.name.to_string(),
                    line: *sym.line_range.start(),
                    children: symbols
                        .iter()
                        .filter(|c| c.parent_id == Some(sym.id))
                        .map(|c| OutlineEntry {
                            kind: c.kind,
                            name: c.name.to_string(),
                            line: *c.line_range.start(),
                            children: vec![],
                        })
                        .collect(),
                });
            }
        }

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

        let store = self.store.lock();
        let rel_path = PathBuf::from(&params.0.path);
        let symbols = store.symbols_for_path(&rel_path).unwrap_or_default();

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

        let store = self.store.lock();
        let rel_path = PathBuf::from(&params.0.path);
        let symbols = store.symbols_for_path(&rel_path).unwrap_or_default();

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

        let refs = store.refs_for_ident(sym.name.as_str()).unwrap_or_default();
        let callers: Vec<RefEntry> = refs
            .iter()
            .take(10)
            .map(|r| RefEntry {
                path: r.src_path.clone(),
                line: 0,
                col: None,
                snippet: r.ident.to_string(),
                enclosing_symbol: None,
            })
            .collect();

        let view = ToolOutput::SymbolCard(SymbolCardView {
            path: rel_path,
            qualified_name: sym.qualified_name.to_string(),
            kind: sym.kind,
            signature: sym.signature.clone(),
            doc: sym.doc.clone(),
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
        let store = self.store.lock();

        // Tier 0: exact match via SQLite index
        let mut results = store
            .find_symbols_exact(&params.0.name, 20)
            .unwrap_or_default();

        // Tier 1: Tantivy BM25 structured search
        if results.is_empty() {
            let hits = self.tantivy.search(&params.0.name, 20).unwrap_or_default();
            for hit in &hits {
                if let Some(sym) = store
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
            let fts_results = store
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
        let store = self.store.lock();
        let refs = store.refs_for_ident(&params.0.symbol).unwrap_or_default();

        let top_k: Vec<RefEntry> = refs
            .iter()
            .take(20)
            .map(|r| RefEntry {
                path: r.src_path.clone(),
                line: 0,
                col: None,
                snippet: r.ident.to_string(),
                enclosing_symbol: None,
            })
            .collect();

        let view = ToolOutput::ReferenceDigest(RefDigestView {
            symbol: params.0.symbol.clone(),
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
        let store = self.store.lock();
        let paths = store.all_file_paths().unwrap_or_default();
        drop(store);

        let abs_paths = self.resolve_search_scope(&paths, params.0.filter.as_deref());

        // Semantic mode via embed feature
        #[cfg(feature = "embed")]
        if params.0.mode == "semantic" {
            // Embed the query under lock, then drop locks before async search
            let embed_result = {
                let mut eg = self.embedder.lock();
                let vg = self.vector_store.lock();
                match (eg.as_mut(), vg.as_ref()) {
                    (Some(embedder), Some(vs)) => match embedder.embed_one(&params.0.query) {
                        Ok(v) => Some((v, vs.clone())),
                        Err(e) => return format!("embed error: {e}"),
                    },
                    _ => None,
                }
            };
            // Locks dropped — safe to await
            if let Some((query_vec, vs)) = embed_result {
                let results = match vs.search(query_vec, None, 20).await {
                    Ok(r) => r,
                    Err(e) => return format!("vector search error: {e}"),
                };
                let entries: Vec<serde_json::Value> = results
                    .iter()
                    .map(|(id, dist)| serde_json::json!({"symbol_id": id, "distance": dist}))
                    .collect();
                return redact_response(
                    serde_json::to_string(&entries).unwrap_or_else(|e| format!("Error: {e}")),
                );
            } else {
                return "semantic search requires embed feature to be initialized (run init_embed)"
                    .into();
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

            let mut rrf: std::collections::HashMap<String, (f64, serde_json::Value)> =
                std::collections::HashMap::new();
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
        let store = self.store.lock();

        // Single query for all files + symbol counts + top symbols
        let summaries = store.file_symbol_summaries().unwrap_or_default();
        drop(store);

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
        let store = self.store.lock();
        let focus_files = params.0.focus_files.as_deref().unwrap_or(&[]);
        let budget = params.0.token_budget;

        // Cache key: last_indexed_at:budget — invalidates when any file is reindexed
        if focus_files.is_empty() {
            let last_idx = store.max_indexed_at().unwrap_or(0);
            let cache_key = format!("map_cache:{}:{}", last_idx, budget);
            if let Ok(Some(cached)) = store.get_meta(&cache_key) {
                drop(store);
                return cached;
            }

            // Compute, cache, return
            let all_symbols = store.all_symbols().unwrap_or_default();
            let all_refs = store.all_refs().unwrap_or_default();

            let ranked = pagerank::pagerank(&all_symbols, &all_refs, &[], 0.85, 30);
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

            if let Err(e) = store.delete_meta_prefix("map_cache:") {
                warn!("failed to clear map cache: {e}");
            }
            if let Err(e) = store.set_meta(&cache_key, &result) {
                warn!("failed to write map cache: {e}");
            }
            drop(store);
            return result;
        }

        // Focused map: no caching, compute fresh
        let all_symbols = store.all_symbols().unwrap_or_default();
        let all_refs = store.all_refs().unwrap_or_default();
        drop(store);

        let focus_set: std::collections::HashSet<&str> =
            focus_files.iter().map(|s| s.as_str()).collect();

        let focus_indices: Vec<usize> = all_symbols
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                let p = s.path.to_string_lossy();
                focus_set.iter().any(|f| p.contains(f))
            })
            .map(|(i, _)| i)
            .collect();

        let ranked = pagerank::pagerank(&all_symbols, &all_refs, &focus_indices, 0.85, 30);
        let content = pagerank::render_repo_map(&all_symbols, &ranked, budget);

        let token_est = recon_search::tokens::estimate_tokens(&content);
        let view = ToolOutput::Skeleton(SkeletonView {
            path: None,
            content,
            token_estimate: token_est,
        });
        redact_response(serde_json::to_string(&view).unwrap_or_else(|e| format!("Error: {e}")))
    }

    #[tool(
        name = "code_find_strings",
        description = "Search for patterns in string literals and comments. Finds SQL fragments, i18n keys, log messages that structural search misses."
    )]
    async fn code_find_strings(&self, params: Parameters<FindStringsParams>) -> String {
        let store = self.store.lock();
        let paths = store.all_file_paths().unwrap_or_default();
        drop(store);

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
        let store = self.store.lock();
        let paths = store.all_file_paths().unwrap_or_default();
        drop(store);

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
        let store = self.store.lock();
        // Explicitly clear map cache before reindexing
        if let Err(e) = store.delete_meta_prefix("map_cache:") {
            warn!("failed to clear map cache on reindex: {e}");
        }
        match indexer::index_repo(&store, Some(&self.tantivy), &self.repo_root) {
            Ok(stats) => serde_json::to_string(&serde_json::json!({
                "status": "ok",
                "files_indexed": stats.files_indexed,
                "total_symbols": stats.total_symbols,
                "errors": stats.errors,
            }))
            .unwrap_or_else(|e| format!("Error: {e}")),
            Err(e) => format!("Reindex failed: {e}"),
        }
    }

    #[tool(
        name = "code_stats",
        description = "Report index health: total files, symbols, last indexed time, Tantivy doc count. Use to check if the index is fresh and complete."
    )]
    async fn code_stats(&self, _params: Parameters<StatsParams>) -> String {
        let store = self.store.lock();
        let file_count = store.all_file_paths().map(|p| p.len()).unwrap_or(0);
        let symbol_count = store.symbol_count().unwrap_or(0);
        let tantivy_docs = self.tantivy.doc_count();
        let schema_version = store
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
