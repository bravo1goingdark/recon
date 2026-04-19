use anyhow::Result;
use clap::{Parser, Subcommand};
use recon_indexer::indexer;
use recon_search::tantivy_backend::TantivyBackend;
use recon_server::server::ReconServer;
use recon_storage::store::Store;
use rmcp::ServiceExt;
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "recon", about = "Token-lean code intelligence MCP server")]
struct Cli {
    /// Repository root path (default: current directory)
    #[arg(long, global = true, default_value = ".")]
    repo: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the MCP server over stdio
    Serve {
        /// Log level
        #[arg(long, default_value = "info")]
        log: String,
    },
    /// Index a repository without starting the server
    Index,
    /// Find symbols by name (fuzzy)
    Find {
        /// Symbol name to search for
        name: String,
        /// Kind filter (fn, struct, class, trait, etc)
        #[arg(short, long)]
        kind: Option<String>,
        /// Language filter (rs, py, ts, go, etc)
        #[arg(short, long)]
        lang: Option<String>,
    },
    /// Search for text patterns in code
    Search {
        /// Search query
        query: String,
        /// Mode: exact (default), regex, hybrid
        #[arg(short, long, default_value = "exact")]
        mode: String,
        /// Filter DSL (e.g. "*.rs", "type:rust", "!test")
        #[arg(short, long)]
        filter: Option<String>,
    },
    /// Show file outline (one line per symbol)
    Outline {
        /// File path relative to repo root
        path: String,
    },
    /// Show file skeleton (signatures, bodies elided)
    Skeleton {
        /// File path relative to repo root
        path: String,
        /// Nesting depth
        #[arg(short, long, default_value = "2")]
        depth: u32,
    },
    /// Read a single symbol's full source
    Symbol {
        /// File path relative to repo root
        path: String,
        /// Symbol name or line number
        name: String,
    },
    /// Find references to a symbol
    Refs {
        /// Symbol name or qualified name
        symbol: String,
    },
    /// List indexed files
    Ls {
        /// Glob pattern
        #[arg(short, long)]
        glob: Option<String>,
        /// Language filter
        #[arg(short, long)]
        lang: Option<String>,
        /// Filter DSL
        #[arg(short, long)]
        filter: Option<String>,
    },
    /// Show PageRank-ranked repo overview
    Map {
        /// Token budget
        #[arg(short, long, default_value = "2000")]
        budget: usize,
        /// Focus files (boost ranking for these)
        #[arg(short, long)]
        focus: Vec<String>,
    },
    /// Search string literals and comments
    Strings {
        /// Pattern to search for
        pattern: String,
        /// Kind: literal, comment, or both (default)
        #[arg(short, long, default_value = "both")]
        kind: String,
        /// Filter DSL
        #[arg(short, long)]
        filter: Option<String>,
    },
    /// Search multiple patterns at once
    Multi {
        /// Patterns to search for
        patterns: Vec<String>,
    },
    /// Show index health stats
    Stats,
    /// Force full re-index
    Reindex,
    /// Raw tool query (JSON args)
    Query {
        /// Tool name (e.g. code_find_symbol)
        tool: String,
        /// Tool arguments as JSON
        #[arg(default_value = "{}")]
        args: String,
    },
}

fn init_server(repo: PathBuf) -> Result<(ReconServer, PathBuf)> {
    let repo = repo.canonicalize()?;
    let store_dir = repo.join(".recon");
    std::fs::create_dir_all(&store_dir)?;

    let store = Store::open(&store_dir.join("index.db")).map_err(|e| anyhow::anyhow!("{e}"))?;
    let tantivy =
        TantivyBackend::open(&store_dir.join("tantivy")).map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok((ReconServer::new(repo.clone(), store, tantivy), repo))
}

/// Index and return the server, suppressing logs for query commands.
async fn query_server(repo: PathBuf) -> Result<ReconServer> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::new("warn"))
        .init();

    let (server, _) = init_server(repo)?;
    server
        .index_repo()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(server)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let repo = cli.repo;

    match cli.command {
        Command::Serve { log } => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(EnvFilter::new(&log))
                .init();

            let (server, repo) = init_server(repo)?;
            info!(?repo, "starting recon server");

            server
                .index_repo()
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            server.start_watcher();

            let (stdin, stdout) = rmcp::transport::io::stdio();
            let _service = server.serve((stdin, stdout)).await?;
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        Command::Index => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(EnvFilter::new("info"))
                .init();

            let repo = repo.canonicalize()?;
            let store_dir = repo.join(".recon");
            std::fs::create_dir_all(&store_dir)?;
            let store =
                Store::open(&store_dir.join("index.db")).map_err(|e| anyhow::anyhow!("{e}"))?;
            let tantivy = TantivyBackend::open(&store_dir.join("tantivy"))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let stats = indexer::index_repo_incremental(&store, Some(&tantivy), &repo)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            eprintln!(
                "Indexed {} files, {} symbols, {} tantivy docs ({} errors)",
                stats.files_indexed,
                stats.total_symbols,
                tantivy.doc_count(),
                stats.errors
            );
            Ok(())
        }
        Command::Find { name, kind, lang } => {
            let server = query_server(repo).await?;
            let args = serde_json::json!({ "name": name, "kind": kind, "lang": lang });
            println!(
                "{}",
                server
                    .query_tool("code_find_symbol", &args.to_string())
                    .await
            );
            Ok(())
        }
        Command::Search {
            query,
            mode,
            filter,
        } => {
            let server = query_server(repo).await?;
            let args = serde_json::json!({ "query": query, "mode": mode, "filter": filter });
            println!(
                "{}",
                server.query_tool("code_search", &args.to_string()).await
            );
            Ok(())
        }
        Command::Outline { path } => {
            let server = query_server(repo).await?;
            let args = serde_json::json!({ "path": path });
            println!(
                "{}",
                server.query_tool("code_outline", &args.to_string()).await
            );
            Ok(())
        }
        Command::Skeleton { path, depth } => {
            let server = query_server(repo).await?;
            let args = serde_json::json!({ "path": path, "depth": depth });
            println!(
                "{}",
                server.query_tool("code_skeleton", &args.to_string()).await
            );
            Ok(())
        }
        Command::Symbol { path, name } => {
            let server = query_server(repo).await?;
            let args = serde_json::json!({ "path": path, "symbol_or_line": name });
            println!(
                "{}",
                server
                    .query_tool("code_read_symbol", &args.to_string())
                    .await
            );
            Ok(())
        }
        Command::Refs { symbol } => {
            let server = query_server(repo).await?;
            let args = serde_json::json!({ "symbol": symbol });
            println!(
                "{}",
                server.query_tool("code_find_refs", &args.to_string()).await
            );
            Ok(())
        }
        Command::Ls { glob, lang, filter } => {
            let server = query_server(repo).await?;
            let args = serde_json::json!({ "glob": glob, "lang": lang, "filter": filter });
            println!(
                "{}",
                server.query_tool("code_list", &args.to_string()).await
            );
            Ok(())
        }
        Command::Map { budget, focus } => {
            let server = query_server(repo).await?;
            let focus = if focus.is_empty() { None } else { Some(focus) };
            let args = serde_json::json!({ "focus_files": focus, "token_budget": budget });
            println!(
                "{}",
                server.query_tool("code_repo_map", &args.to_string()).await
            );
            Ok(())
        }
        Command::Strings {
            pattern,
            kind,
            filter,
        } => {
            let server = query_server(repo).await?;
            let args = serde_json::json!({ "pattern": pattern, "kind": kind, "filter": filter });
            println!(
                "{}",
                server
                    .query_tool("code_find_strings", &args.to_string())
                    .await
            );
            Ok(())
        }
        Command::Multi { patterns } => {
            let server = query_server(repo).await?;
            let args = serde_json::json!({ "patterns": patterns });
            println!(
                "{}",
                server
                    .query_tool("code_multi_find", &args.to_string())
                    .await
            );
            Ok(())
        }
        Command::Stats => {
            let server = query_server(repo).await?;
            println!("{}", server.query_tool("code_stats", "{}").await);
            Ok(())
        }
        Command::Reindex => {
            let server = query_server(repo).await?;
            println!("{}", server.query_tool("code_reindex", "{}").await);
            Ok(())
        }
        Command::Query { tool, args } => {
            let server = query_server(repo).await?;
            println!("{}", server.query_tool(&tool, &args).await);
            Ok(())
        }
    }
}
