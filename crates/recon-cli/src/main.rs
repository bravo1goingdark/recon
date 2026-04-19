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
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the MCP server over stdio
    Serve {
        /// Repository root path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Log level
        #[arg(long, default_value = "info")]
        log: String,
    },
    /// Index a repository without starting the server
    Index {
        /// Repository root path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Query a tool directly without an MCP client
    Query {
        /// Repository root path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Tool name (e.g. code_find_symbol, code_outline, code_search)
        tool: String,
        /// Tool arguments as JSON (e.g. '{"name":"main"}')
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve { repo, log } => {
            // Log to stderr only — stdout is the MCP JSON-RPC channel
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
        Command::Index { repo } => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(EnvFilter::new("info"))
                .init();

            let (_, repo) = {
                let repo_path = repo.canonicalize()?;
                let store_dir = repo_path.join(".recon");
                std::fs::create_dir_all(&store_dir)?;
                let store =
                    Store::open(&store_dir.join("index.db")).map_err(|e| anyhow::anyhow!("{e}"))?;
                let tantivy = TantivyBackend::open(&store_dir.join("tantivy"))
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let stats = indexer::index_repo_incremental(&store, Some(&tantivy), &repo_path)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                eprintln!(
                    "Indexed {} files, {} symbols, {} tantivy docs ({} errors)",
                    stats.files_indexed,
                    stats.total_symbols,
                    tantivy.doc_count(),
                    stats.errors
                );
                (store, repo_path)
            };
            let _ = repo;
            Ok(())
        }
        Command::Query { repo, tool, args } => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(EnvFilter::new("warn"))
                .init();

            let (server, _) = init_server(repo)?;

            // Index first (incremental — fast if already indexed)
            server
                .index_repo()
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            let result = server.query_tool(&tool, &args).await;
            println!("{result}");
            Ok(())
        }
    }
}
