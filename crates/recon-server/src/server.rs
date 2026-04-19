//! MCP server implementation using rmcp.
//!
//! All built components wired: Tantivy for structured search, PageRank for
//! repo-map, secret redaction on all responses, spawn_blocking for SQLite.

use crate::tools::*;
use recon_core::lang::Language;
use recon_core::redact;
use recon_core::shapes::*;
use recon_indexer::indexer;
use recon_search::{fuzzy, pagerank, text};
use recon_storage::store::Store;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

/// The recon MCP server.
#[derive(Clone)]
pub struct ReconServer {
    tool_router: ToolRouter<Self>,
    store: Arc<Mutex<Store>>,
    repo_root: PathBuf,
}

/// Apply secret redaction to a tool response string.
fn redact_response(response: String) -> String {
    redact::redact_secrets(&response).unwrap_or(response)
}

impl ReconServer {
    /// Create a new server for the given repo root.
    pub fn new(repo_root: PathBuf, store: Store) -> Self {
        Self {
            tool_router: Self::tool_router(),
            store: Arc::new(Mutex::new(store)),
            repo_root,
        }
    }

    /// Run initial indexing of the repo.
    pub async fn index_repo(&self) -> Result<(), recon_core::error::Error> {
        let store = self.store.lock().await;
        let stats = indexer::index_repo(&store, &self.repo_root)?;
        info!(
            files = stats.files_indexed,
            symbols = stats.total_symbols,
            "initial indexing complete"
        );
        Ok(())
    }

    /// Resolve and validate a path within the repo root.
    fn resolve_path(&self, rel: &str) -> Result<PathBuf, String> {
        if redact::is_blocked_path(std::path::Path::new(rel)) {
            return Err(format!("access denied: sensitive file: {rel}"));
        }
        let path = self.repo_root.join(rel);
        let canonical = path
            .canonicalize()
            .map_err(|e| format!("path not found: {rel}: {e}"))?;
        let root_canonical = self
            .repo_root
            .canonicalize()
            .map_err(|e| format!("repo root error: {e}"))?;
        if !canonical.starts_with(&root_canonical) {
            return Err(format!("path traversal denied: {rel}"));
        }
        Ok(canonical)
    }
}

#[tool_router(router = tool_router)]
impl ReconServer {
    #[tool(
        name = "code_outline",
        description = "Show one-line-per-symbol outline of a file. Returns symbol kinds, names, and line numbers in a tree structure. Use instead of Read when you need to understand a file's structure without reading its full content. Typical output: 300-500 tokens for a 500-line file."
    )]
    async fn code_outline(&self, params: Parameters<OutlineParams>) -> String {
        let store = self.store.lock().await;
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
        redact_response(serde_json::to_string_pretty(&view).unwrap_or_else(|e| format!("Error: {e}")))
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

        let store = self.store.lock().await;
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

        let token_est = skeleton.len() / 4;
        let view = ToolOutput::Skeleton(SkeletonView {
            path: Some(rel_path),
            content: skeleton,
            token_estimate: token_est,
        });
        redact_response(serde_json::to_string_pretty(&view).unwrap_or_else(|e| format!("Error: {e}")))
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

        let store = self.store.lock().await;
        let rel_path = PathBuf::from(&params.0.path);
        let symbols = store.symbols_for_path(&rel_path).unwrap_or_default();

        let target = if let Ok(line) = params.0.symbol_or_line.parse::<u32>() {
            symbols.iter().find(|s| s.line_range.contains(&line))
        } else {
            symbols.iter().find(|s| s.name.as_str() == params.0.symbol_or_line)
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
        redact_response(serde_json::to_string_pretty(&view).unwrap_or_else(|e| format!("Error: {e}")))
    }

    #[tool(
        name = "code_find_symbol",
        description = "Find symbols by name across the codebase. Supports exact, fuzzy, and BM25 matching. Use instead of Grep when searching for functions, types, or classes. Returns qualified names, paths, and signatures."
    )]
    async fn code_find_symbol(&self, params: Parameters<FindSymbolParams>) -> String {
        let store = self.store.lock().await;

        // Tier 0: exact match via SQLite index
        let mut results = store.find_symbols_exact(&params.0.name, 20).unwrap_or_default();

        // Tier 1: fuzzy via FTS5 trigram + nucleo rescore
        if results.is_empty() {
            let fts_results = store.search_symbols_fuzzy(&params.0.name, 50).unwrap_or_default();
            let ranked = fuzzy::fuzzy_rank(&fts_results, &params.0.name, 20);
            results = ranked.into_iter().map(|(i, _)| fts_results[i].clone()).collect();
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

        serde_json::to_string_pretty(&entries).unwrap_or_else(|e| format!("Error: {e}"))
    }

    #[tool(
        name = "code_find_refs",
        description = "Find all references to a symbol. Returns a count and top-k call sites as path:line triples. Use instead of Grep for finding usages of a function or type."
    )]
    async fn code_find_refs(&self, params: Parameters<FindRefsParams>) -> String {
        let store = self.store.lock().await;
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
        serde_json::to_string_pretty(&view).unwrap_or_else(|e| format!("Error: {e}"))
    }

    #[tool(
        name = "code_search",
        description = "Search for text patterns across the codebase. Modes: exact (default), regex. Use instead of Grep for code search. Returns path, line, and matching text."
    )]
    async fn code_search(&self, params: Parameters<SearchParams>) -> String {
        let store = self.store.lock().await;
        let paths = store.all_file_paths().unwrap_or_default();
        drop(store); // release lock before I/O

        let abs_paths: Vec<PathBuf> = paths
            .iter()
            .map(|p| self.repo_root.join(p))
            .collect();

        let is_regex = params.0.mode == "regex";
        let hits = text::search_files(&params.0.query, &abs_paths, is_regex, 30)
            .unwrap_or_default();

        let entries: Vec<serde_json::Value> = hits
            .iter()
            .map(|h| {
                let rel = h.path.strip_prefix(&self.repo_root).unwrap_or(&h.path);
                serde_json::json!({
                    "path": rel.to_string_lossy(),
                    "line": h.line,
                    "col": h.col,
                    "text": h.line_text,
                })
            })
            .collect();

        redact_response(serde_json::to_string_pretty(&entries).unwrap_or_else(|e| format!("Error: {e}")))
    }

    #[tool(
        name = "code_list",
        description = "List indexed source files with language, line count, and top symbols. Use instead of Glob when you need structured file listings. Supports language filter."
    )]
    async fn code_list(&self, params: Parameters<ListParams>) -> String {
        let store = self.store.lock().await;
        let paths = store.all_file_paths().unwrap_or_default();

        let mut entries: Vec<serde_json::Value> = Vec::with_capacity(paths.len());
        for path in &paths {
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

            let syms = store.symbols_for_path(path).unwrap_or_default();
            let top_syms: Vec<String> = syms
                .iter()
                .filter(|s| s.parent_id.is_none())
                .take(3)
                .map(|s| format!("{} {}", s.kind.label(), s.name))
                .collect();

            entries.push(serde_json::json!({
                "path": path.to_string_lossy(),
                "lang": lang.name(),
                "symbol_count": syms.len(),
                "top_symbols": top_syms,
            }));
        }

        serde_json::to_string_pretty(&entries).unwrap_or_else(|e| format!("Error: {e}"))
    }

    #[tool(
        name = "code_repo_map",
        description = "Generate a ranked overview of the most important symbols in the repo. Uses personalized PageRank over the reference graph with Aider-style edge weights. Output fits within a token budget (default 2000). Best first tool to call for orientation."
    )]
    async fn code_repo_map(&self, params: Parameters<RepoMapParams>) -> String {
        let store = self.store.lock().await;
        let paths = store.all_file_paths().unwrap_or_default();

        // Collect all symbols and refs for PageRank
        let mut all_symbols = Vec::new();
        let mut all_refs = Vec::new();
        for path in &paths {
            let syms = store.symbols_for_path(path).unwrap_or_default();
            all_symbols.extend(syms);
        }
        // Collect refs for all top-level symbol names
        let mut seen_idents = std::collections::HashSet::new();
        for sym in &all_symbols {
            if seen_idents.insert(sym.name.as_str()) {
                all_refs.extend(store.refs_for_ident(sym.name.as_str()).unwrap_or_default());
            }
        }

        // Determine focus symbol indices
        let focus_indices: Vec<usize> = params
            .0
            .focus_files
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .flat_map(|f| {
                all_symbols
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.path.to_string_lossy().contains(f.as_str()))
                    .map(|(i, _)| i)
            })
            .collect();

        // Run PageRank
        let ranked = pagerank::pagerank(&all_symbols, &all_refs, &focus_indices, 0.85, 30);
        let content = pagerank::render_repo_map(&all_symbols, &ranked, params.0.token_budget);

        let token_est = content.len() / 4;
        let view = ToolOutput::Skeleton(SkeletonView {
            path: None,
            content,
            token_estimate: token_est,
        });
        serde_json::to_string_pretty(&view).unwrap_or_else(|e| format!("Error: {e}"))
    }

    #[tool(
        name = "code_find_strings",
        description = "Search for patterns in string literals and comments. Finds SQL fragments, i18n keys, log messages that structural search misses. Use for non-code text search."
    )]
    async fn code_find_strings(&self, params: Parameters<FindStringsParams>) -> String {
        let store = self.store.lock().await;
        let paths = store.all_file_paths().unwrap_or_default();
        drop(store);

        let abs_paths: Vec<PathBuf> = paths
            .iter()
            .map(|p| self.repo_root.join(p))
            .collect();

        let hits = text::search_files(&params.0.pattern, &abs_paths, false, 30)
            .unwrap_or_default();

        let entries: Vec<serde_json::Value> = hits
            .iter()
            .map(|h| {
                let rel = h.path.strip_prefix(&self.repo_root).unwrap_or(&h.path);
                serde_json::json!({
                    "path": rel.to_string_lossy(),
                    "line": h.line,
                    "text": h.line_text,
                    "kind": params.0.kind,
                })
            })
            .collect();

        redact_response(serde_json::to_string_pretty(&entries).unwrap_or_else(|e| format!("Error: {e}")))
    }

    #[tool(
        name = "code_multi_find",
        description = "Search for multiple patterns at once. More efficient than multiple code_search calls. Returns results grouped by pattern."
    )]
    async fn code_multi_find(&self, params: Parameters<MultiFindParams>) -> String {
        let store = self.store.lock().await;
        let paths = store.all_file_paths().unwrap_or_default();
        drop(store);

        let abs_paths: Vec<PathBuf> = paths
            .iter()
            .map(|p| self.repo_root.join(p))
            .collect();

        let mut results: Vec<serde_json::Value> = Vec::with_capacity(params.0.patterns.len());
        for pattern in &params.0.patterns {
            let hits = text::search_files(pattern, &abs_paths, false, 10)
                .unwrap_or_default();

            let entries: Vec<serde_json::Value> = hits
                .iter()
                .map(|h| {
                    let rel = h.path.strip_prefix(&self.repo_root).unwrap_or(&h.path);
                    serde_json::json!({
                        "path": rel.to_string_lossy(),
                        "line": h.line,
                        "text": h.line_text,
                    })
                })
                .collect();

            results.push(serde_json::json!({
                "pattern": pattern,
                "hits": entries,
            }));
        }

        redact_response(serde_json::to_string_pretty(&results).unwrap_or_else(|e| format!("Error: {e}")))
    }
}

#[tool_handler]
impl ServerHandler for ReconServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::default()
            .with_server_info(Implementation::new("recon", env!("CARGO_PKG_VERSION")))
    }
}
