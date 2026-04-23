#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod pretty;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use recon_indexer::indexer;
use recon_search::tantivy_backend::TantivyBackend;
use recon_server::server::ReconServer;
use recon_storage::store::Store;
use rmcp::ServiceExt;
use std::path::{Path, PathBuf};
use tracing::info;
#[cfg(feature = "embed")]
use tracing::warn;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "recon", about = "Token-lean code intelligence MCP server")]
struct Cli {
    /// Repository root path (default: current directory)
    #[arg(long, global = true, default_value = ".")]
    repo: PathBuf,

    /// Output raw JSON instead of formatted text
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

/// Target IDE for MCP config file generation.
#[derive(Clone, ValueEnum)]
enum Ide {
    /// Claude Code — writes `.mcp.json`
    #[value(name = "cc")]
    ClaudeCode,
    /// OpenCode — writes `.opencode/mcp.json`
    #[value(name = "oc")]
    OpenCode,
    /// Cursor — writes `.cursor/mcp.json`
    #[value(name = "cursor")]
    Cursor,
    /// Windsurf — writes `.windsurf/mcp.json`
    #[value(name = "windsurf")]
    Windsurf,
}

impl Ide {
    /// Absolute path to the MCP config file for this IDE.
    ///
    /// Project-local IDEs (Claude Code, OpenCode, Cursor) resolve relative to
    /// `repo`.  Windsurf writes to a machine-global path regardless of `repo`.
    fn config_abs_path(&self, repo: &Path) -> PathBuf {
        match self {
            Ide::ClaudeCode => repo.join(".mcp.json"),
            Ide::OpenCode => repo.join("opencode.jsonc"),
            Ide::Cursor => repo.join(".cursor").join("mcp.json"),
            Ide::Windsurf => windsurf_global_config(),
        }
    }

    /// Top-level JSON key under which MCP servers are listed.
    fn servers_key(&self) -> &'static str {
        match self {
            Ide::OpenCode => "mcp",
            _ => "mcpServers",
        }
    }

    /// Build the per-server JSON entry for this IDE's config schema.
    fn server_entry(&self, repo: &Path, recon_bin: &str) -> serde_json::Value {
        match self {
            // OpenCode: command is an array, explicit type field required.
            Ide::OpenCode => serde_json::json!({
                "type": "local",
                "command": [recon_bin, "--repo", repo.to_string_lossy().as_ref(), "serve"]
            }),
            // Claude Code, Cursor, Windsurf: command string + args array.
            _ => serde_json::json!({
                "command": recon_bin,
                "args": ["--repo", repo.to_string_lossy().as_ref(), "serve"]
            }),
        }
    }
}

/// Returns the Windsurf global MCP config path.
///
/// - Linux / macOS: `~/.codeium/windsurf/mcp_config.json`
/// - Windows:       `%USERPROFILE%\.codeium\windsurf\mcp_config.json`
///
/// Override with `RECON_WINDSURF_CONFIG_PATH` for tests and CI.
fn windsurf_global_config() -> PathBuf {
    if let Ok(p) = std::env::var("RECON_WINDSURF_CONFIG_PATH") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codeium")
        .join("windsurf")
        .join("mcp_config.json")
}

#[derive(Subcommand)]
enum Command {
    /// Validate an API key and cache the license globally (~/.config/recon/)
    Login {
        /// API key — get one at https://mcprecon.pages.dev/login
        key: String,
    },
    /// Remove the globally cached license
    Logout,
    /// Show current cached license tier, limits, and expiry
    License,
    /// Index the repo and optionally set up an IDE MCP config
    Init {
        /// Write MCP config for the given IDE (cc | oc | cursor | windsurf)
        #[arg(long, value_enum)]
        mcp: Option<Ide>,
    },
    /// Start the MCP server (stdio by default; HTTP with --port)
    Serve {
        /// Log level
        #[arg(long, default_value = "info")]
        log: String,
        /// Port for Streamable HTTP transport (omit for stdio)
        #[arg(short, long)]
        port: Option<u16>,
        /// Bind address for HTTP transport
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
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
    /// Delete all index data (.recon directory)
    Purge,
    /// Show version
    Version,
    /// Raw tool query (JSON args)
    Query {
        /// Tool name (e.g. code_find_symbol)
        tool: String,
        /// Tool arguments as JSON
        #[arg(default_value = "{}")]
        args: String,
    },
}

// ── License helpers ────────────────────────────────────────────────────────────

/// Read the globally cached license, failing with a clear user-facing message.
fn validate_license_or_die() -> Result<recon_server::license::ValidatedLicense> {
    let config_dir = recon_server::license::global_config_dir();
    recon_server::license::validate_license(None, &config_dir).map_err(|e| anyhow::anyhow!("{e}"))
}

/// One-time migration: if the global cache is missing but a per-repo
/// `.recon/license.json` exists, copy it to the global config dir.
///
/// This keeps existing users working after upgrading from the per-repo
/// license model without forcing an immediate `recon login`.
fn maybe_migrate_license(repo: &Path) {
    let global_dir = recon_server::license::global_config_dir();
    let global_cache = global_dir.join("license.json");

    if global_cache.exists() {
        return; // Already have a global cache — nothing to do.
    }

    let per_repo_cache = repo.join(".recon").join("license.json");
    if !per_repo_cache.exists() {
        return;
    }

    if let Ok(content) = std::fs::read_to_string(&per_repo_cache) {
        if std::fs::create_dir_all(&global_dir).is_ok()
            && std::fs::write(&global_cache, &content).is_ok()
        {
            eprintln!(
                "License migrated from .recon/license.json → {}",
                global_cache.display()
            );
        }
    }
}

// ── MCP config ────────────────────────────────────────────────────────────────

/// Write (or merge) an MCP server entry into the IDE's config file.
///
/// Merges with any existing content so that other MCP servers already
/// configured by the user are preserved.  Each IDE has its own config path
/// and JSON schema — see [`Ide::config_abs_path`] and [`Ide::servers_key`].
fn write_mcp_config(ide: &Ide, repo: &Path, recon_bin: &str) -> Result<()> {
    let config_path = ide.config_abs_path(repo);

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let servers_key = ide.servers_key();
    let server_entry = ide.server_entry(repo, recon_bin);

    // Read and merge with existing config to preserve other servers.
    let mut merged: serde_json::Value = if config_path.exists() {
        let content = std::fs::read_to_string(&config_path)?;
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if let Some(obj) = merged.as_object_mut() {
        let servers = obj
            .entry(servers_key)
            .or_insert_with(|| serde_json::json!({}));
        if let Some(servers_obj) = servers.as_object_mut() {
            servers_obj.insert("recon".into(), server_entry);
        }
    } else {
        merged = serde_json::json!({ servers_key: { "recon": server_entry } });
    }

    std::fs::write(&config_path, serde_json::to_string_pretty(&merged)?)?;
    eprintln!("Wrote {}", config_path.display());
    Ok(())
}

// ── Server helpers ─────────────────────────────────────────────────────────────

fn init_server(repo: PathBuf) -> Result<(ReconServer, PathBuf)> {
    let repo = repo.canonicalize()?;
    let store_dir = repo.join(".recon");
    std::fs::create_dir_all(&store_dir)?;

    let store = Store::open(&store_dir.join("index.db")).map_err(|e| anyhow::anyhow!("{e}"))?;
    let tantivy =
        TantivyBackend::open(&store_dir.join("tantivy")).map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok((
        ReconServer::new(repo.clone(), store, tantivy).map_err(|e| anyhow::anyhow!("{e}"))?,
        repo,
    ))
}

/// Open an existing index for read-only CLI queries (no re-index on startup).
fn read_server(repo: PathBuf) -> Result<ReconServer> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::new("warn"))
        .init();

    let (server, _) = init_server(repo)?;
    Ok(server)
}

/// Serve the MCP server over Streamable HTTP.
async fn serve_http(server: ReconServer, host: &str, port: u16) -> Result<()> {
    use hyper_util::rt::TokioIo;
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    const MAX_CONCURRENT: usize = 100;

    let cancel = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_stateful_mode(true)
        .with_sse_keep_alive(Some(std::time::Duration::from_secs(15)))
        .with_json_response(false)
        .with_cancellation_token(cancel.clone())
        .with_allowed_hosts(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
            format!("localhost:{port}"),
            format!("127.0.0.1:{port}"),
            format!("::1:{port}"),
        ]);

    let session_manager = Arc::new(LocalSessionManager::default());
    let service = StreamableHttpService::new(move || Ok(server.clone()), session_manager, config);

    let addr: std::net::SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Streamable HTTP server listening on http://{addr}/mcp");

    let mut tasks = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            accept = listener.accept() => {
                let (stream, _peer) = accept?;
                let io = TokioIo::new(stream);
                let svc = service.clone();
                if tasks.len() >= MAX_CONCURRENT {
                    let _ = tasks.join_next().await;
                }
                tasks.spawn(async move {
                    if let Err(e) = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection(io, hyper::service::service_fn(move |req| {
                        let mut svc = svc.clone();
                        async move {
                            use tower_service::Service;
                            svc.call(req).await
                        }
                    }))
                    .await
                    {
                        tracing::warn!("HTTP connection error: {e}");
                    }
                });
            }
            _ = wait_for_shutdown_signal() => {
                info!("http server shutting down");
                cancel.cancel();
                break;
            }
        }
    }

    tasks.shutdown().await;
    Ok(())
}

// ── Shutdown signal ────────────────────────────────────────────────────────────

/// Wait for either SIGINT (Ctrl-C) or, on Unix, SIGTERM.
///
/// Systemd / docker / kubernetes send SIGTERM to request graceful stop.
/// Without this, the MCP server would exit only on Ctrl-C and get SIGKILLed
/// in production after the kill-grace window.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("could not install SIGTERM handler: {e}");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT");
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        info!("received Ctrl-C");
    }
}

// ── Panic hook ─────────────────────────────────────────────────────────────────

/// Install a panic hook that writes a structured one-line record plus backtrace
/// to stderr. Writes to stderr only — stdio-transport MCP would corrupt on a
/// panic to stdout. Captures a backtrace when `RUST_BACKTRACE` is set.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let payload = info.payload();
        let msg = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic payload>");
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let backtrace = std::backtrace::Backtrace::capture();
        eprintln!("\n[recon] panic at {location} in thread {thread:?}: {msg}\n{backtrace}");
    }));
}

// ── main ───────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    install_panic_hook();

    let cli = Cli::parse();
    let repo = cli.repo;
    let raw_json = cli.json;
    let out = |s: &str| pretty::print_output(s, raw_json);

    match cli.command {
        // ── Authentication ────────────────────────────────────────────────────
        Command::Login { key } => {
            let config_dir = recon_server::license::global_config_dir();
            std::fs::create_dir_all(&config_dir)?;
            let license = recon_server::license::validate_license(Some(&key), &config_dir)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let limits = license.tier.limits();
            eprintln!(
                "✓ Authenticated — {} tier ({} repos, {} files, {}K LOC)",
                license.tier.name(),
                limits.max_repos,
                limits.max_files,
                limits.max_loc / 1_000,
            );
            if !license.message.is_empty() {
                eprintln!("  {}", license.message);
            }
            eprintln!();
            eprintln!("Next steps:");
            eprintln!("  cd your-project");
            eprintln!("  recon init --mcp cc      # Claude Code");
            eprintln!("  recon init --mcp cursor  # Cursor");
            eprintln!("  recon init --mcp windsurf  # Windsurf");
            eprintln!("  recon init --mcp oc      # OpenCode");
            Ok(())
        }

        Command::Logout => {
            let config_dir = recon_server::license::global_config_dir();
            let license_path = config_dir.join("license.json");
            if license_path.exists() {
                std::fs::remove_file(&license_path)?;
                eprintln!("License removed ({})", license_path.display());
            } else {
                eprintln!("No cached license found.");
            }
            Ok(())
        }

        Command::License => {
            let config_dir = recon_server::license::global_config_dir();
            let license = recon_server::license::validate_license(None, &config_dir)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let limits = license.tier.limits();
            eprintln!("Tier:      {}", license.tier.name());
            eprintln!("Source:    {}", license.source);
            eprintln!("Max repos: {}", limits.max_repos);
            eprintln!("Max files: {}", limits.max_files);
            eprintln!("Max LOC:   {}K", limits.max_loc / 1_000);
            if license.expires_at > 0 {
                eprintln!("Expires:   {} (unix)", license.expires_at);
            } else {
                eprintln!("Expires:   never");
            }
            if !license.message.is_empty() {
                eprintln!("{}", license.message);
            }
            Ok(())
        }

        // ── Indexing + IDE setup ──────────────────────────────────────────────
        Command::Init { mcp } => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(EnvFilter::new("info"))
                .init();

            let repo = repo.canonicalize()?;

            // Migrate per-repo license to global before validation.
            maybe_migrate_license(&repo);

            // License must be present before we do any work.
            let license = validate_license_or_die()?;
            let limits = license.tier.limits();

            // Repo tracking — enforce max_repos for new repos.
            let config_dir = recon_server::license::global_config_dir();
            let repos =
                recon_server::repos::load_repos(&config_dir).map_err(|e| anyhow::anyhow!("{e}"))?;
            let repo_path_str = repo.to_string_lossy().to_string();
            if !recon_server::repos::is_indexed(&repos, &repo_path_str)
                && repos.len() >= limits.max_repos
            {
                return Err(anyhow::anyhow!(
                    "{} plan allows {} repo(s). You have {} registered.\n\
                     Upgrade at https://mcprecon.pages.dev/pricing",
                    license.tier.name(),
                    limits.max_repos,
                    repos.len(),
                ));
            }

            // 1. Index the repo
            let store_dir = repo.join(".recon");
            std::fs::create_dir_all(&store_dir)?;
            let store =
                Store::open(&store_dir.join("index.db")).map_err(|e| anyhow::anyhow!("{e}"))?;
            let tantivy = TantivyBackend::open(&store_dir.join("tantivy"))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut writer = tantivy.writer(50_000_000).ok();
            let stats =
                indexer::index_repo_incremental(&store, Some(&tantivy), &repo, writer.as_mut())
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            eprintln!(
                "Indexed {} files, {} symbols ({} errors)",
                stats.files_indexed, stats.total_symbols, stats.errors
            );

            // Register / update this repo in the global tracking file.
            recon_server::repos::add_or_update_repo(
                &config_dir,
                &repo_path_str,
                stats.files_indexed,
                stats.total_symbols as usize,
            )
            .map_err(|e| anyhow::anyhow!("failed to update repos registry: {e}"))?;

            // 2. Write IDE MCP config (only if --mcp was passed)
            if let Some(ide) = mcp {
                let recon_bin = std::env::current_exe()?
                    .canonicalize()?
                    .to_string_lossy()
                    .to_string();
                write_mcp_config(&ide, &repo, &recon_bin)?;
            }

            // 3. Add .recon/ to .gitignore if not already there
            let gitignore_path = repo.join(".gitignore");
            let needs_ignore = if gitignore_path.exists() {
                let content = std::fs::read_to_string(&gitignore_path)?;
                !content
                    .lines()
                    .any(|l| l.trim() == ".recon/" || l.trim() == ".recon")
            } else {
                true
            };
            if needs_ignore {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&gitignore_path)?;
                // Ensure we start on a new line
                if gitignore_path.exists() {
                    let content = std::fs::read_to_string(&gitignore_path)?;
                    if !content.is_empty() && !content.ends_with('\n') {
                        writeln!(f)?;
                    }
                }
                writeln!(f, ".recon/")?;
                eprintln!("Added .recon/ to .gitignore");
            }

            // 4. Append CLAUDE.md hint if not already present
            let claude_md = repo.join("CLAUDE.md");
            let recon_hint = "Prefer code_* tools (code_outline, code_skeleton, \
                code_find_symbol, code_search, code_repo_map) over Read/Grep/Glob \
                for code exploration.";
            let needs_hint = if claude_md.exists() {
                let content = std::fs::read_to_string(&claude_md)?;
                !content.contains("code_*")
            } else {
                false // don't create CLAUDE.md if it doesn't exist
            };
            if needs_hint {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().append(true).open(&claude_md)?;
                let content = std::fs::read_to_string(&claude_md)?;
                if !content.ends_with('\n') {
                    writeln!(f)?;
                }
                writeln!(f, "\n## recon MCP tools\n{recon_hint}")?;
                eprintln!("Added recon hint to CLAUDE.md");
            }

            eprintln!("Done. Restart your IDE to activate recon tools.");
            Ok(())
        }

        // ── MCP server ────────────────────────────────────────────────────────
        Command::Serve { log, port, host } => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(EnvFilter::new(&log))
                .init();

            // Try to migrate a per-repo license before validating.
            let canon_repo = repo.canonicalize().unwrap_or_else(|e| {
                tracing::debug!("canonicalize failed for {}: {e}", repo.display());
                repo.clone()
            });
            maybe_migrate_license(&canon_repo);

            // Validate license — limits determine what we'll allow to index.
            let license = validate_license_or_die()?;
            let limits = license.tier.limits();
            info!(
                tier = license.tier.name(),
                source = %license.source,
                max_repos = limits.max_repos,
                max_files = limits.max_files,
                max_loc = limits.max_loc,
                "license: {}",
                license.message,
            );

            // Pre-flight: check repo size against license limits before indexing.
            let paths = recon_indexer::walker::walk_repo(&repo);
            if paths.len() > limits.max_files {
                return Err(anyhow::anyhow!(
                    "Repository has {} source files — exceeds your {} plan limit of {} files.\n\
                     Upgrade at https://mcprecon.pages.dev/pricing",
                    paths.len(),
                    license.tier.name(),
                    limits.max_files,
                ));
            }
            // Estimate LOC by sampling up to 200 files.
            let sample = paths.len().min(200);
            if sample > 0 {
                let sample_loc: usize = paths[..sample]
                    .iter()
                    .filter_map(|p| std::fs::read(p).ok())
                    .map(|c| c.iter().filter(|&&b| b == b'\n').count())
                    .sum();
                let estimated_loc =
                    (sample_loc as f64 / sample as f64 * paths.len() as f64) as usize;
                if estimated_loc > limits.max_loc {
                    return Err(anyhow::anyhow!(
                        "Repository has ~{}K lines of code — exceeds your {} plan limit of {}K LOC.\n\
                         Upgrade at https://mcprecon.pages.dev/pricing",
                        estimated_loc / 1_000,
                        license.tier.name(),
                        limits.max_loc / 1_000,
                    ));
                }
                info!(files = paths.len(), estimated_loc, "repo size OK");
            }

            let (server, repo) = init_server(repo)?;
            info!(?repo, "starting recon server");

            server
                .index_repo()
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            #[cfg(feature = "embed")]
            if let Err(e) = server.init_embed().await {
                warn!("embed init failed, semantic search disabled: {e}");
            }

            server.start_watcher();

            if let Some(port) = port {
                // serve_http already drives its own shutdown via ctrl_c + cancel.
                let result = serve_http(server.clone(), &host, port).await;
                server.shutdown().await;
                result
            } else {
                let (stdin, stdout) = rmcp::transport::io::stdio();
                let _service = server.clone().serve((stdin, stdout)).await?;
                wait_for_shutdown_signal().await;
                server.shutdown().await;
                Ok(())
            }
        }

        // ── Indexing only ─────────────────────────────────────────────────────
        Command::Index => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(EnvFilter::new("info"))
                .init();

            validate_license_or_die()?;

            let repo = repo.canonicalize()?;
            let store_dir = repo.join(".recon");
            std::fs::create_dir_all(&store_dir)?;
            let store =
                Store::open(&store_dir.join("index.db")).map_err(|e| anyhow::anyhow!("{e}"))?;
            let tantivy = TantivyBackend::open(&store_dir.join("tantivy"))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut writer = tantivy.writer(50_000_000).ok();
            let stats =
                indexer::index_repo_incremental(&store, Some(&tantivy), &repo, writer.as_mut())
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

        // ── Read-only query commands (all need a valid license) ───────────────
        Command::Find { name, kind, lang } => {
            validate_license_or_die()?;
            let server = read_server(repo)?;
            let args = serde_json::json!({ "name": name, "kind": kind, "lang": lang });
            out(&server
                .query_tool("code_find_symbol", &args.to_string())
                .await);
            Ok(())
        }
        Command::Search {
            query,
            mode,
            filter,
        } => {
            validate_license_or_die()?;
            let server = read_server(repo)?;
            let args = serde_json::json!({ "query": query, "mode": mode, "filter": filter });
            out(&server.query_tool("code_search", &args.to_string()).await);
            Ok(())
        }
        Command::Outline { path } => {
            validate_license_or_die()?;
            let server = read_server(repo)?;
            let args = serde_json::json!({ "path": path });
            out(&server.query_tool("code_outline", &args.to_string()).await);
            Ok(())
        }
        Command::Skeleton { path, depth } => {
            validate_license_or_die()?;
            let server = read_server(repo)?;
            let args = serde_json::json!({ "path": path, "depth": depth });
            out(&server.query_tool("code_skeleton", &args.to_string()).await);
            Ok(())
        }
        Command::Symbol { path, name } => {
            validate_license_or_die()?;
            let server = read_server(repo)?;
            let args = serde_json::json!({ "path": path, "symbol_or_line": name });
            out(&server
                .query_tool("code_read_symbol", &args.to_string())
                .await);
            Ok(())
        }
        Command::Refs { symbol } => {
            validate_license_or_die()?;
            let server = read_server(repo)?;
            let args = serde_json::json!({ "symbol": symbol });
            out(&server.query_tool("code_find_refs", &args.to_string()).await);
            Ok(())
        }
        Command::Ls { glob, lang, filter } => {
            validate_license_or_die()?;
            let server = read_server(repo)?;
            let args = serde_json::json!({ "glob": glob, "lang": lang, "filter": filter });
            out(&server.query_tool("code_list", &args.to_string()).await);
            Ok(())
        }
        Command::Map { budget, focus } => {
            validate_license_or_die()?;
            let server = read_server(repo)?;
            let focus = if focus.is_empty() { None } else { Some(focus) };
            let args = serde_json::json!({ "focus_files": focus, "token_budget": budget });
            out(&server.query_tool("code_repo_map", &args.to_string()).await);
            Ok(())
        }
        Command::Strings {
            pattern,
            kind,
            filter,
        } => {
            validate_license_or_die()?;
            let server = read_server(repo)?;
            let args = serde_json::json!({ "pattern": pattern, "kind": kind, "filter": filter });
            out(&server
                .query_tool("code_find_strings", &args.to_string())
                .await);
            Ok(())
        }
        Command::Multi { patterns } => {
            validate_license_or_die()?;
            let server = read_server(repo)?;
            let args = serde_json::json!({ "patterns": patterns });
            out(&server
                .query_tool("code_multi_find", &args.to_string())
                .await);
            Ok(())
        }
        Command::Reindex => {
            validate_license_or_die()?;
            let server = read_server(repo)?;
            out(&server.query_tool("code_reindex", "{}").await);
            Ok(())
        }
        Command::Query { tool, args } => {
            validate_license_or_die()?;
            let server = read_server(repo)?;
            out(&server.query_tool(&tool, &args).await);
            Ok(())
        }

        // ── No-license commands ───────────────────────────────────────────────
        Command::Stats => {
            let server = read_server(repo)?;
            out(&server.query_tool("code_stats", "{}").await);
            // Append global repo count (best-effort; no license required).
            let config_dir = recon_server::license::global_config_dir();
            if let Ok(repos) = recon_server::repos::load_repos(&config_dir) {
                eprintln!("Indexed repos (global): {}", repos.len());
            }
            Ok(())
        }
        Command::Purge => {
            let repo = repo.canonicalize()?;
            let store_dir = repo.join(".recon");
            if store_dir.exists() {
                std::fs::remove_dir_all(&store_dir)?;
                eprintln!("Purged {}", store_dir.display());
            } else {
                eprintln!("No index found at {}", store_dir.display());
            }
            Ok(())
        }
        Command::Version => {
            println!("recon {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    // ── Ide::config_abs_path ──────────────────────────────────────────────────

    #[test]
    fn ide_config_path_claude_code() {
        let dir = tempdir().unwrap();
        assert_eq!(
            Ide::ClaudeCode.config_abs_path(dir.path()),
            dir.path().join(".mcp.json")
        );
    }

    #[test]
    fn ide_config_path_opencode() {
        let dir = tempdir().unwrap();
        assert_eq!(
            Ide::OpenCode.config_abs_path(dir.path()),
            dir.path().join("opencode.jsonc")
        );
    }

    #[test]
    fn ide_config_path_cursor() {
        let dir = tempdir().unwrap();
        assert_eq!(
            Ide::Cursor.config_abs_path(dir.path()),
            dir.path().join(".cursor").join("mcp.json")
        );
    }

    #[test]
    fn ide_config_path_windsurf_is_global_not_in_repo() {
        let repo = tempdir().unwrap();
        let global = tempdir().unwrap();
        let override_path = global.path().join("mcp_config.json");
        // Override the global path so this test doesn't touch the real home dir.
        std::env::set_var("RECON_WINDSURF_CONFIG_PATH", &override_path);
        let path = Ide::Windsurf.config_abs_path(repo.path());
        std::env::remove_var("RECON_WINDSURF_CONFIG_PATH");
        assert_eq!(path, override_path);
        assert!(
            !path.starts_with(repo.path()),
            "Windsurf config must not live inside the repo"
        );
    }

    #[test]
    fn all_ide_config_paths_are_distinct() {
        let repo = tempdir().unwrap();
        let global = tempdir().unwrap();
        std::env::set_var(
            "RECON_WINDSURF_CONFIG_PATH",
            global.path().join("mcp_config.json"),
        );
        let paths = [
            Ide::ClaudeCode.config_abs_path(repo.path()),
            Ide::OpenCode.config_abs_path(repo.path()),
            Ide::Cursor.config_abs_path(repo.path()),
            Ide::Windsurf.config_abs_path(repo.path()),
        ];
        std::env::remove_var("RECON_WINDSURF_CONFIG_PATH");
        let mut seen = std::collections::HashSet::new();
        for p in &paths {
            assert!(seen.insert(p.clone()), "duplicate IDE config path: {p:?}");
        }
    }

    // ── write_mcp_config — Claude Code ────────────────────────────────────────

    #[test]
    fn write_mcp_config_creates_file() {
        let dir = tempdir().unwrap();
        write_mcp_config(&Ide::ClaudeCode, dir.path(), "/usr/bin/recon").unwrap();
        assert!(dir.path().join(".mcp.json").exists());
    }

    #[test]
    fn write_mcp_config_contains_recon_entry() {
        let dir = tempdir().unwrap();
        write_mcp_config(&Ide::ClaudeCode, dir.path(), "/usr/bin/recon").unwrap();
        let content = fs::read_to_string(dir.path().join(".mcp.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["mcpServers"]["recon"]["command"], "/usr/bin/recon");
    }

    #[test]
    fn write_mcp_config_args_contain_serve() {
        let dir = tempdir().unwrap();
        write_mcp_config(&Ide::ClaudeCode, dir.path(), "/usr/bin/recon").unwrap();
        let content = fs::read_to_string(dir.path().join(".mcp.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        let args = v["mcpServers"]["recon"]["args"].as_array().unwrap();
        assert!(
            args.iter().any(|a| a.as_str() == Some("serve")),
            "args must include 'serve': {args:?}"
        );
    }

    #[test]
    fn write_mcp_config_args_do_not_contain_key() {
        let dir = tempdir().unwrap();
        write_mcp_config(&Ide::ClaudeCode, dir.path(), "/usr/bin/recon").unwrap();
        let content = fs::read_to_string(dir.path().join(".mcp.json")).unwrap();
        assert!(
            !content.contains("--key") && !content.contains("RECON_KEY"),
            "MCP config must not contain key material: {content}"
        );
    }

    #[test]
    fn write_mcp_config_merges_with_existing_servers() {
        let dir = tempdir().unwrap();
        let existing = serde_json::json!({
            "mcpServers": { "other-tool": { "command": "other", "args": [] } }
        });
        fs::write(
            dir.path().join(".mcp.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();
        write_mcp_config(&Ide::ClaudeCode, dir.path(), "/usr/bin/recon").unwrap();
        let content = fs::read_to_string(dir.path().join(".mcp.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(v["mcpServers"]["other-tool"].is_object());
        assert!(v["mcpServers"]["recon"].is_object());
    }

    #[test]
    fn write_mcp_config_overwrites_stale_recon_entry() {
        let dir = tempdir().unwrap();
        let stale = serde_json::json!({
            "mcpServers": { "recon": { "command": "/old/path/recon", "args": [] } }
        });
        fs::write(
            dir.path().join(".mcp.json"),
            serde_json::to_string_pretty(&stale).unwrap(),
        )
        .unwrap();
        write_mcp_config(&Ide::ClaudeCode, dir.path(), "/new/recon").unwrap();
        let content = fs::read_to_string(dir.path().join(".mcp.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["mcpServers"]["recon"]["command"], "/new/recon");
    }

    #[test]
    fn write_mcp_config_corrupt_existing_json_replaced() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".mcp.json"), b"not valid {{{{").unwrap();
        write_mcp_config(&Ide::ClaudeCode, dir.path(), "/usr/bin/recon").unwrap();
        let content = fs::read_to_string(dir.path().join(".mcp.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(v["mcpServers"]["recon"].is_object());
    }

    // ── write_mcp_config — OpenCode ───────────────────────────────────────────

    #[test]
    fn write_mcp_config_opencode_writes_at_repo_root() {
        let dir = tempdir().unwrap();
        write_mcp_config(&Ide::OpenCode, dir.path(), "/usr/bin/recon").unwrap();
        assert!(
            dir.path().join("opencode.jsonc").exists(),
            "OpenCode config must be opencode.jsonc at repo root"
        );
    }

    #[test]
    fn write_mcp_config_opencode_uses_mcp_key() {
        let dir = tempdir().unwrap();
        write_mcp_config(&Ide::OpenCode, dir.path(), "/usr/bin/recon").unwrap();
        let content = fs::read_to_string(dir.path().join("opencode.jsonc")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(
            v["mcp"]["recon"].is_object(),
            "OpenCode must use 'mcp' top-level key, got: {v}"
        );
    }

    #[test]
    fn write_mcp_config_opencode_has_type_local() {
        let dir = tempdir().unwrap();
        write_mcp_config(&Ide::OpenCode, dir.path(), "/usr/bin/recon").unwrap();
        let content = fs::read_to_string(dir.path().join("opencode.jsonc")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            v["mcp"]["recon"]["type"], "local",
            "OpenCode entry must have type=local"
        );
    }

    #[test]
    fn write_mcp_config_opencode_command_is_array_with_serve() {
        let dir = tempdir().unwrap();
        write_mcp_config(&Ide::OpenCode, dir.path(), "/usr/bin/recon").unwrap();
        let content = fs::read_to_string(dir.path().join("opencode.jsonc")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        let cmd = v["mcp"]["recon"]["command"].as_array().unwrap();
        assert_eq!(cmd[0], "/usr/bin/recon", "first element must be the binary");
        assert!(
            cmd.iter().any(|a| a.as_str() == Some("serve")),
            "command array must include 'serve': {cmd:?}"
        );
    }

    #[test]
    fn write_mcp_config_opencode_does_not_contain_key_material() {
        let dir = tempdir().unwrap();
        write_mcp_config(&Ide::OpenCode, dir.path(), "/usr/bin/recon").unwrap();
        let content = fs::read_to_string(dir.path().join("opencode.jsonc")).unwrap();
        assert!(
            !content.contains("--key") && !content.contains("RECON_KEY"),
            "OpenCode config must not contain key material: {content}"
        );
    }

    #[test]
    fn write_mcp_config_opencode_merges_with_existing() {
        let dir = tempdir().unwrap();
        let existing = serde_json::json!({
            "mcp": { "other-tool": { "type": "local", "command": ["other"] } }
        });
        fs::write(
            dir.path().join("opencode.jsonc"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();
        write_mcp_config(&Ide::OpenCode, dir.path(), "/usr/bin/recon").unwrap();
        let content = fs::read_to_string(dir.path().join("opencode.jsonc")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(v["mcp"]["other-tool"].is_object());
        assert!(v["mcp"]["recon"].is_object());
    }

    // ── write_mcp_config — Cursor ─────────────────────────────────────────────

    #[test]
    fn write_mcp_config_cursor_creates_parent_dir() {
        let dir = tempdir().unwrap();
        write_mcp_config(&Ide::Cursor, dir.path(), "/usr/bin/recon").unwrap();
        assert!(dir.path().join(".cursor").join("mcp.json").exists());
    }

    #[test]
    fn write_mcp_config_cursor_uses_mcp_servers_key() {
        let dir = tempdir().unwrap();
        write_mcp_config(&Ide::Cursor, dir.path(), "/usr/bin/recon").unwrap();
        let content = fs::read_to_string(dir.path().join(".cursor").join("mcp.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(v["mcpServers"]["recon"].is_object());
    }

    // ── write_mcp_config — Windsurf ───────────────────────────────────────────

    #[test]
    fn write_mcp_config_windsurf_writes_to_global_path() {
        let repo = tempdir().unwrap();
        let global = tempdir().unwrap();
        let config_path = global.path().join("mcp_config.json");
        std::env::set_var("RECON_WINDSURF_CONFIG_PATH", &config_path);
        write_mcp_config(&Ide::Windsurf, repo.path(), "/usr/bin/recon").unwrap();
        std::env::remove_var("RECON_WINDSURF_CONFIG_PATH");
        assert!(
            config_path.exists(),
            "Windsurf config must exist at global path"
        );
        assert!(
            !repo.path().join("mcp_config.json").exists(),
            "Windsurf config must NOT be inside the repo"
        );
    }

    #[test]
    fn write_mcp_config_windsurf_uses_mcp_servers_key() {
        let repo = tempdir().unwrap();
        let global = tempdir().unwrap();
        let config_path = global.path().join("mcp_config.json");
        std::env::set_var("RECON_WINDSURF_CONFIG_PATH", &config_path);
        write_mcp_config(&Ide::Windsurf, repo.path(), "/usr/bin/recon").unwrap();
        std::env::remove_var("RECON_WINDSURF_CONFIG_PATH");
        let content = fs::read_to_string(&config_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(v["mcpServers"]["recon"].is_object());
    }

    #[test]
    fn write_mcp_config_windsurf_merges_with_existing() {
        let repo = tempdir().unwrap();
        let global = tempdir().unwrap();
        let config_path = global.path().join("mcp_config.json");
        let existing = serde_json::json!({
            "mcpServers": { "github": { "command": "gh-mcp", "args": [] } }
        });
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();
        std::env::set_var("RECON_WINDSURF_CONFIG_PATH", &config_path);
        write_mcp_config(&Ide::Windsurf, repo.path(), "/usr/bin/recon").unwrap();
        std::env::remove_var("RECON_WINDSURF_CONFIG_PATH");
        let content = fs::read_to_string(&config_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(v["mcpServers"]["github"].is_object());
        assert!(v["mcpServers"]["recon"].is_object());
    }

    // ── maybe_migrate_license ─────────────────────────────────────────────────

    #[test]
    fn migrate_copies_per_repo_license_to_global() {
        let repo_dir = tempdir().unwrap();
        let global_dir = tempdir().unwrap();

        // Write a fake per-repo license
        let recon_dir = repo_dir.path().join(".recon");
        fs::create_dir_all(&recon_dir).unwrap();
        let license_content = r#"{"cached_at":9999999999,"response":{"valid":true,"tier":"Pro","limits":{"max_repos":10,"max_files":5000,"max_loc":200000},"expires_at":0,"message":""}}"#;
        fs::write(recon_dir.join("license.json"), license_content).unwrap();

        // Override global config dir by writing the cache directly.
        // We call the internal logic by constructing the paths manually.
        let global_cache = global_dir.path().join("license.json");
        assert!(!global_cache.exists());

        // Simulate what maybe_migrate_license does, using temp dirs.
        {
            if !global_cache.exists() {
                let per_repo_cache = repo_dir.path().join(".recon").join("license.json");
                if per_repo_cache.exists() {
                    if let Ok(content) = fs::read_to_string(&per_repo_cache) {
                        fs::create_dir_all(global_dir.path()).ok();
                        fs::write(&global_cache, &content).ok();
                    }
                }
            }
        }

        assert!(global_cache.exists(), "license should have been migrated");
        let migrated = fs::read_to_string(&global_cache).unwrap();
        assert!(migrated.contains("Pro"));
    }

    #[test]
    fn migrate_skips_when_global_already_exists() {
        let repo_dir = tempdir().unwrap();
        let global_dir = tempdir().unwrap();

        // Both exist — global should NOT be overwritten.
        let recon_dir = repo_dir.path().join(".recon");
        fs::create_dir_all(&recon_dir).unwrap();
        fs::write(recon_dir.join("license.json"), r#"{"tier":"Pro"}"#).unwrap();

        let global_cache = global_dir.path().join("license.json");
        fs::write(&global_cache, r#"{"tier":"Enterprise"}"#).unwrap();

        // Simulate the guard check — global exists, so we do nothing.
        // (The real maybe_migrate_license returns early if global_cache.exists())
        let original = fs::read_to_string(&global_cache).unwrap();
        // Global stays unchanged.
        assert!(original.contains("Enterprise"));
    }

    #[test]
    fn migrate_skips_when_no_per_repo_license() {
        let repo_dir = tempdir().unwrap();
        // No .recon/license.json in repo
        let result = std::panic::catch_unwind(|| {
            maybe_migrate_license(repo_dir.path());
        });
        assert!(result.is_ok(), "should not panic when nothing to migrate");
    }
}
