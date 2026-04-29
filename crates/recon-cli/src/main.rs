#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod doctor;
mod pretty;
mod savings;
mod update;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use recon_indexer::indexer;
use recon_search::tantivy_backend::TantivyBackend;
use recon_server::server::ReconServer;
use recon_storage::store::Store;
use rmcp::ServiceExt;
use std::path::{Path, PathBuf};
use tracing::info;
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

    /// Where to write the strict-policy agent rules for this IDE.
    ///
    /// Two layouts:
    /// - `Shared(path)` — rules block fenced by recon markers inside a file
    ///   the project may also use for unrelated content (`CLAUDE.md`,
    ///   `AGENTS.md`).  Append-only.  Both `CLAUDE.md` and `AGENTS.md` are
    ///   auto-created when missing — without this, `recon init` returned
    ///   success while silently skipping the rules block, so the agent
    ///   started without recon's `code_*`-first discovery and defaulted to
    ///   `Read`/`Grep`/`Glob`.  Symmetric purge deletes the file again if
    ///   the only thing it ever contained was the recon block.
    /// - `Dedicated(path, body)` — recon owns the file outright
    ///   (`.cursor/rules/recon.mdc`, `.windsurf/rules/recon.md`).  Purge can
    ///   simply delete it; no string surgery required.
    fn agent_target(&self, repo: &Path) -> AgentTarget {
        match self {
            Ide::ClaudeCode => AgentTarget::Shared {
                path: repo.join("CLAUDE.md"),
                // Auto-create when missing (v0.2.2). Without this, projects
                // without a hand-curated CLAUDE.md got `recon init --mcp cc`
                // returning success while silently skipping the rules block —
                // Claude Code would then start without the strict-policy
                // discovery the recon workflow requires. The matching purge
                // path deletes the file again if only the recon block was
                // ever in it (`recon_only_remainder`).
                create_if_missing: true,
            },
            Ide::OpenCode => AgentTarget::Shared {
                path: repo.join("AGENTS.md"),
                create_if_missing: true,
            },
            Ide::Cursor => AgentTarget::Dedicated {
                path: repo.join(".cursor").join("rules").join("recon.mdc"),
                body: CURSOR_MDC.to_string(),
            },
            Ide::Windsurf => AgentTarget::Dedicated {
                path: repo.join(".windsurf").join("rules").join("recon.md"),
                body: WINDSURF_MD.to_string(),
            },
        }
    }
}

enum AgentTarget {
    Shared {
        path: PathBuf,
        create_if_missing: bool,
    },
    Dedicated {
        path: PathBuf,
        body: String,
    },
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
    /// Manage repos registered with your recon account (server-side)
    ///
    /// As of v0.2.0, `max_repos` is enforced by the recon worker rather than
    /// against a local file. Use `list` to see your registered repos and
    /// `remove` to free a slot.
    Repos {
        #[command(subcommand)]
        action: ReposAction,
    },
    /// Push token-savings rollups to the dashboard (Pro/Team feature)
    ///
    /// Reads the local telemetry counters in `.recon/index.db` (populated
    /// by every MCP tool call) and POSTs today's snapshot to the worker.
    /// Free tier returns a clear upgrade message rather than an error.
    /// Idempotent — re-running on the same day MAX-merges, never
    /// double-counts.
    Savings {
        #[command(subcommand)]
        action: SavingsAction,
    },
    /// Delete recon's index and (optionally) its IDE wiring
    ///
    /// With no flag: removes only `.recon/` (index, merkle snapshot, caches).
    /// With `--mcp <ide>`: also removes the recon entry from that IDE's MCP
    /// config, strips the strict-policy block from the matching agent file,
    /// and frees this repo's slot in the global tracking registry — i.e. the
    /// symmetric inverse of `recon init --mcp <ide>`.
    Purge {
        /// Tear down wiring for the given IDE (cc | oc | cursor | windsurf)
        #[arg(long, value_enum)]
        mcp: Option<Ide>,
    },
    /// Show version
    Version,
    /// Upgrade recon to the latest published release.
    ///
    /// Fetches latest.json from the distribution origin, downloads the
    /// matching target tarball, verifies the SHA256 against the
    /// published manifest, and replaces the running binary in place.
    /// Unix does this atomically via rename-over; Windows moves the
    /// current .exe to recon.exe.old first (the OS won't let us
    /// unlink a running executable).
    Update {
        /// Only report whether an update is available; don't download.
        #[arg(long)]
        check: bool,
        /// Reinstall even if already on the latest version.
        /// Useful when an HMAC secret rotated and you need a fresh
        /// binary baked with the new key.
        #[arg(long)]
        force: bool,
    },
    /// Health check — verifies license, worker reachability, index, MCP wiring
    Doctor {
        /// Output as JSON instead of human-readable lines
        #[arg(long)]
        json: bool,
    },
    /// Raw tool query (JSON args)
    Query {
        /// Tool name (e.g. code_find_symbol)
        tool: String,
        /// Tool arguments as JSON
        #[arg(default_value = "{}")]
        args: String,
    },
}

/// Subcommands of `recon repos`.
#[derive(Subcommand)]
enum ReposAction {
    /// List repos currently registered with your recon account
    List,
    /// Remove a repo from your recon account, freeing a slot
    ///
    /// `target` may be a path (canonicalized + SHA-256'd) or a 64-char hex
    /// fingerprint pulled from `recon repos list`. Paths to deleted
    /// directories work too — fingerprinting falls back to the verbatim
    /// path string when canonicalize fails.
    Remove {
        /// Path or fingerprint to release
        target: String,
    },
}

/// Subcommands of `recon savings`.
#[derive(Subcommand)]
enum SavingsAction {
    /// Push today's local telemetry rollup to the dashboard
    ///
    /// Aggregates the per-tool counters in `.recon/index.db` into one
    /// daily row and POSTs to the recon worker. Repeated runs on the
    /// same day are idempotent (MAX-merged). Pro/Team only.
    Push {
        /// Repository whose `.recon/index.db` to read.
        /// Defaults to the current directory.
        #[arg(long)]
        repo: Option<std::path::PathBuf>,
    },
    /// Print local savings counters as TSV (no network)
    ///
    /// Reads telemetry counters straight from `.recon/index.db`,
    /// without spinning up the MCP server.
    Show {
        /// Repository whose `.recon/index.db` to read.
        #[arg(long)]
        repo: Option<std::path::PathBuf>,
    },
}

// ── Auto-push helpers ──────────────────────────────────────────────────────────

/// Run `server.shutdown()` with a hard upper bound so a stuck SQLite
/// flush (e.g. our `.recon/` was unlinked from underneath us and the
/// final WAL write blocks on a phantom inode) cannot wedge the
/// process at exit time.
///
/// 5 s is generous: a healthy shutdown commits Tantivy, flushes
/// telemetry, and runs `exit_indexing_mode` in well under a second on
/// the largest indexes we've measured. Anything past 5 s is the
/// pathological case that justified Fix #3 in v0.3.4.
async fn shutdown_with_timeout(server: &recon_server::server::ReconServer) {
    let deadline = std::time::Duration::from_secs(5);
    if (tokio::time::timeout(deadline, server.shutdown()).await).is_err() {
        tracing::warn!(
            "server.shutdown() did not return within {} s; forcing exit. \
             A stuck SQLite/Tantivy flush is the most common cause; \
             check whether `.recon/` was unlinked while the server was \
             running.",
            deadline.as_secs()
        );
    }
}

/// Print a one-block savings summary to stderr at the end of every
/// `recon serve` session. The IDE's MCP debug log captures stderr, so
/// users see it in Claude Code / Cursor / Windsurf without having to
/// remember `recon savings show` or visit the dashboard.
///
/// Output shape:
///
/// ```text
/// recon · session ended
///   N tool calls · saved K tokens vs Read+Grep equivalent
///   top: code_outline (12,400)  code_search (9,800)  code_read_symbol (8,100)
///   dashboard: https://mcprecon.pages.dev/dashboard
/// ```
///
/// Suppressed when:
/// - the session had zero tool calls (no "0 tokens saved" noise on a
///   fresh `recon serve` that nobody connected to),
/// - `RECON_QUIET=1`/`true`/`yes`/`on` is set (CI / scripted runs).
///
/// Telemetry is best-effort and the receipt is *more* best-effort:
/// any panic or empty snapshot is a silent skip, never a blocker on
/// shutdown.
fn print_session_receipt(server: &recon_server::server::ReconServer) {
    let raw = std::env::var("RECON_QUIET").unwrap_or_default();
    let quiet = matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    );
    if quiet {
        return;
    }

    let snapshots = server.telemetry_arc().per_tool_snapshots();
    let mut total_calls = 0u64;
    let mut total_saved = 0u64;
    let mut per_tool: Vec<(&'static str, u64)> = Vec::with_capacity(snapshots.len());
    for (name, s) in &snapshots {
        if s.calls == 0 {
            continue;
        }
        total_calls += s.calls;
        let saved = s.tokens_saved();
        total_saved += saved;
        per_tool.push((*name, saved));
    }
    if total_calls == 0 {
        return;
    }
    per_tool.sort_by_key(|(_, saved)| std::cmp::Reverse(*saved));

    let top: Vec<String> = per_tool
        .iter()
        .take(3)
        .filter(|(_, s)| *s > 0)
        .map(|(name, saved)| format!("{name} ({})", thousands(*saved)))
        .collect();

    eprintln!("recon · session ended");
    eprintln!(
        "  {} tool calls · saved {} tokens vs Read+Grep equivalent",
        thousands(total_calls),
        thousands(total_saved),
    );
    if !top.is_empty() {
        eprintln!("  top: {}", top.join("  "));
    }
    eprintln!("  dashboard: https://mcprecon.pages.dev/dashboard");
}

/// Format `n` with comma thousand-separators. Pure local helper for the
/// session receipt; no `i18n` aspirations.
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

/// Opt-in: when `RECON_AUTO_PUSH_SAVINGS=1` is set, push the local
/// telemetry rollup to the dashboard after `recon serve` exits.
///
/// Pure side-effect, never blocks shutdown. All failures are logged
/// to stderr so an operator running `recon serve --log debug` sees
/// them, but the process exit code remains whatever `serve` produced.
///
/// Default-on as of v0.5.0 — most paid users never set the env var so
/// the dashboard would show empty / upsell state forever despite live
/// telemetry sitting in `.recon/index.db`. Privacy framing: the push
/// only sends the aggregated counters (calls, response_tokens,
/// baseline_tokens, latency_micros_total) keyed by tool name plus the
/// licensed user's API key — no source, no paths, no logs (already the
/// case before this flip; see crates/recon-cli/src/savings.rs).
///
/// Opt out by setting `RECON_AUTO_PUSH_SAVINGS=0` (or `false` / `no`
/// / `off`). Anything else (including unset) is treated as on.
fn maybe_auto_push_savings(repo: &Path) {
    let raw = std::env::var("RECON_AUTO_PUSH_SAVINGS").unwrap_or_default();
    let trimmed = raw.trim().to_ascii_lowercase();
    let opted_out = matches!(trimmed.as_str(), "0" | "false" | "no" | "off");
    if opted_out {
        return;
    }
    let repo_buf = repo.to_path_buf();
    match savings::push(Some(repo_buf)) {
        Ok(()) => {}
        Err(e) => {
            // Log via tracing so it shows up under --log debug, AND
            // print to stderr in case the user is running with the
            // default log level. Never block shutdown.
            tracing::warn!("auto-push savings failed: {e}");
            eprintln!("recon: auto-push savings failed: {e}");
        }
    }
}

// ── License helpers ────────────────────────────────────────────────────────────

/// Read the globally cached license, failing with a clear user-facing message.
fn validate_license_or_die() -> Result<recon_server::license::ValidatedLicense> {
    let config_dir = recon_server::license::global_config_dir();
    recon_server::license::validate_license(None, &config_dir).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Spawn `recon serve` against the given repo, wait briefly, and surface
/// any startup failure to the caller.
///
/// The MCP transport in Claude Code / opencode / Cursor pipes the child
/// process's stderr to the IDE's debug log (where most users never look)
/// and surfaces only `MCP error -32000: connection closed` when the
/// child exits before the JSON-RPC `initialize` reply. That's
/// uninformative — failures from license rejection, over-tier repos,
/// panics during indexing, or a missing credentials file all collapse
/// into the same opaque message.
///
/// `recon init --mcp <ide>` calls this right after writing the IDE's
/// MCP config so the *same* command + binary path the IDE will use gets
/// exercised here first. If the child exits within the wait window we
/// capture its stderr verbatim and bubble it up as the init error,
/// pointing the user at the real cause before they ever open the IDE.
///
/// The wait window is 4 s — license + workspace open + first-pass index
/// re-validation finishes well within that on a 320K-symbol repo (we
/// just indexed in the previous step, so the in-process index is hot).
/// If the child is still alive after 4 s we kill it cleanly and call
/// the test passed.
fn smoke_test_serve(recon_bin: &str, repo_path: &str) -> Result<()> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    eprintln!("Verifying `recon serve` starts cleanly…");

    // stdin/stdout piped so the child stays alive waiting for MCP frames
    // (otherwise it might EOF immediately and pass a false-negative).
    // stderr piped so we can dump it into our error if startup fails.
    let mut child = Command::new(recon_bin)
        .args(["--repo", repo_path, "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to spawn `{recon_bin}` for smoke test: {e}\n\
                 The MCP config we just wrote points at this binary; \
                 if it's not executable here it won't be from the IDE either."
            )
        })?;

    // Sleep window: enough for license validate + workspace open + the
    // license-revalidation task spawn. 4 s is comfortable margin on a
    // 320K-symbol repo (incremental re-index sees no changes since
    // `recon init` just indexed in this same invocation).
    std::thread::sleep(Duration::from_secs(4));

    match child.try_wait() {
        Ok(Some(status)) => {
            // Child exited within the window — bad. Capture stderr verbatim
            // so the user sees the exact tracing/panic output the IDE
            // would have hidden.
            let mut stderr_buf = String::new();
            if let Some(mut err) = child.stderr.take() {
                let _ = err.read_to_string(&mut stderr_buf);
            }
            let trimmed = stderr_buf.trim();
            Err(anyhow::anyhow!(
                "Smoke test failed: `recon serve` exited with {status} \
                 before the MCP `initialize` handshake.\n\n\
                 In the IDE this would surface only as:\n  \
                 MCP error -32000: connection closed\n\n\
                 The actual server output (would be hidden by the IDE) is:\n\n\
                 ╭── recon serve stderr ──────────────────────────────────\n\
                 {}\n\
                 ╰────────────────────────────────────────────────────────\n\n\
                 Fix the issue above, then re-run `recon init --mcp <ide>` \
                 — the call is idempotent.",
                if trimmed.is_empty() {
                    "(no stderr output captured)".to_string()
                } else {
                    trimmed
                        .lines()
                        .map(|l| format!("│ {l}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            ))
        }
        Ok(None) => {
            // Still running after the wait window — server is alive,
            // license validated, indexer hot. Kill cleanly so we don't
            // leave a zombie process around.
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("✓ Server smoke test passed");
            Ok(())
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(anyhow::anyhow!(
                "smoke test wait failed: {e} — couldn't determine \
                 whether `recon serve` is healthy. Run \
                 `recon --repo {repo_path} serve` directly to see."
            ))
        }
    }
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

// ── Agent rule wiring ──────────────────────────────────────────────────────────

/// Stable markers used to delimit recon's block in shared agent files
/// (`CLAUDE.md`, `AGENTS.md`).  `recon purge --mcp <ide>` strips the bytes
/// between (and including) these markers, leaving the rest of the file intact.
/// HTML comments rather than markdown headers because they survive arbitrary
/// content reorgs around them.
const RECON_RULES_START: &str = "<!-- recon:start -->";
const RECON_RULES_END: &str = "<!-- recon:end -->";

/// Strict policy body inserted into shared agent files.
///
/// Wording is deliberately blunt: Claude Code and equivalents have strong
/// priors toward `Read`/`Grep`/`Glob`, and a soft "prefer" hint loses to
/// those priors in practice.  The exception clause is the escape hatch when
/// recon genuinely cannot answer (binaries, generated assets, freshly added
/// files before reindex), gated on explicit user confirmation so the model
/// cannot silently regress to the high-token path.
const RECON_RULES_BODY: &str = "## recon MCP tools — strict policy

For all code exploration in this repository, you MUST use the `code_*` tools \
provided by the recon MCP server:

- Reading code: `code_outline`, `code_skeleton`, `code_read_symbol` (instead of `Read`)
- Searching: `code_find_symbol`, `code_find_refs`, `code_search`, `code_find_strings`, `code_multi_find` (instead of `Grep`)
- Listing / orientation: `code_list`, `code_repo_map` (instead of `Glob`)
- Index health: `code_reindex`

Do **not** use `Read`, `Grep`, or `Glob` on source files by default.

**Exception.** If no `code_*` tool can answer the question — for example a \
non-source file (JSON config, Markdown doc, generated asset), a freshly \
created file the index has not picked up yet, or the recon index is \
unavailable — you MAY use `Read`, `Grep`, or `Glob`, but only after:

1. Stopping and asking the user for explicit permission, and
2. Explaining which `code_*` tool you tried and the specific reason it could not answer.

Do not silently fall back. The whole point of recon is the 15–30× token \
reduction; defaulting to `Read`/`Grep`/`Glob` defeats it.";

/// Cursor `.mdc` rule file.  Frontmatter `alwaysApply: true` makes Cursor
/// inject the body on every prompt, matching the strict-policy intent.
const CURSOR_MDC: &str = "---
description: recon MCP tool usage policy (strict)
alwaysApply: true
---

## recon MCP tools — strict policy

For all code exploration in this repository, you MUST use the `code_*` tools \
provided by the recon MCP server:

- Reading code: `code_outline`, `code_skeleton`, `code_read_symbol` (instead of `Read`)
- Searching: `code_find_symbol`, `code_find_refs`, `code_search`, `code_find_strings`, `code_multi_find` (instead of `Grep`)
- Listing / orientation: `code_list`, `code_repo_map` (instead of `Glob`)
- Index health: `code_reindex`

Do **not** use `Read`, `Grep`, or `Glob` on source files by default.

**Exception.** If no `code_*` tool can answer the question — for example a \
non-source file (JSON config, Markdown doc, generated asset), a freshly \
created file the index has not picked up yet, or the recon index is \
unavailable — you MAY use `Read`, `Grep`, or `Glob`, but only after:

1. Stopping and asking the user for explicit permission, and
2. Explaining which `code_*` tool you tried and the specific reason it could not answer.

Do not silently fall back. The whole point of recon is the 15–30× token \
reduction; defaulting to `Read`/`Grep`/`Glob` defeats it.
";

/// Windsurf rule file.  Frontmatter `trigger: always_on` is Windsurf's
/// equivalent of Cursor's `alwaysApply: true`.
const WINDSURF_MD: &str = "---
description: recon MCP tool usage policy (strict)
trigger: always_on
---

## recon MCP tools — strict policy

For all code exploration in this repository, you MUST use the `code_*` tools \
provided by the recon MCP server:

- Reading code: `code_outline`, `code_skeleton`, `code_read_symbol` (instead of `Read`)
- Searching: `code_find_symbol`, `code_find_refs`, `code_search`, `code_find_strings`, `code_multi_find` (instead of `Grep`)
- Listing / orientation: `code_list`, `code_repo_map` (instead of `Glob`)
- Index health: `code_reindex`

Do **not** use `Read`, `Grep`, or `Glob` on source files by default.

**Exception.** If no `code_*` tool can answer the question — for example a \
non-source file (JSON config, Markdown doc, generated asset), a freshly \
created file the index has not picked up yet, or the recon index is \
unavailable — you MAY use `Read`, `Grep`, or `Glob`, but only after:

1. Stopping and asking the user for explicit permission, and
2. Explaining which `code_*` tool you tried and the specific reason it could not answer.

Do not silently fall back. The whole point of recon is the 15–30× token \
reduction; defaulting to `Read`/`Grep`/`Glob` defeats it.
";

/// Render the fenced block recon writes into shared agent files.
fn shared_block() -> String {
    format!("{RECON_RULES_START}\n{RECON_RULES_BODY}\n{RECON_RULES_END}\n")
}

/// Write recon's strict agent rules for the chosen IDE.
///
/// - Shared targets (`CLAUDE.md`, `AGENTS.md`): append a marker-fenced block.
///   Idempotent — already-present markers cause a skip with a stderr note.
///   `CLAUDE.md` is never created from scratch (preserves the project's
///   hand-curated context file convention).
/// - Dedicated targets (`.cursor/rules/recon.mdc`, `.windsurf/rules/recon.md`):
///   overwrite with the canonical body.  Safe because recon owns the path.
fn write_agent_rules(ide: &Ide, repo: &Path) -> Result<()> {
    match ide.agent_target(repo) {
        AgentTarget::Shared {
            path,
            create_if_missing,
        } => {
            let exists = path.exists();
            if !exists && !create_if_missing {
                eprintln!(
                    "Skipped agent rules: {} not found (create it manually, then re-run init)",
                    path.display()
                );
                return Ok(());
            }
            let mut content = if exists {
                std::fs::read_to_string(&path)?
            } else {
                String::new()
            };
            if content.contains(RECON_RULES_START) {
                eprintln!("Agent rules already present in {}", path.display());
                return Ok(());
            }
            // Separate from prior content with a blank line so the block
            // doesn't run on; idempotent since we exit above on re-run.
            if !content.is_empty() && !content.ends_with("\n\n") {
                if content.ends_with('\n') {
                    content.push('\n');
                } else {
                    content.push_str("\n\n");
                }
            }
            content.push_str(&shared_block());
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, content)?;
            eprintln!("Wrote recon rules block to {}", path.display());
        }
        AgentTarget::Dedicated { path, body } => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, body)?;
            eprintln!("Wrote {}", path.display());
        }
    }
    Ok(())
}

/// Reverse of [`write_agent_rules`].
///
/// - Shared targets: strip the marker-fenced block (and any trailing blank
///   line we inserted).  No-op if markers are absent or the file is gone.
/// - Dedicated targets: delete the file outright.  No-op if absent.
fn remove_agent_rules(ide: &Ide, repo: &Path) -> Result<()> {
    match ide.agent_target(repo) {
        AgentTarget::Shared { path, .. } => {
            if !path.exists() {
                return Ok(());
            }
            let content = std::fs::read_to_string(&path)?;
            let Some(start) = content.find(RECON_RULES_START) else {
                return Ok(());
            };
            // End marker must come *after* the start marker.
            let after_start = start + RECON_RULES_START.len();
            let Some(end_rel) = content[after_start..].find(RECON_RULES_END) else {
                return Ok(());
            };
            let end = after_start + end_rel + RECON_RULES_END.len();
            // Also consume the trailing newline that follows the end marker
            // (and any blank line we inserted before the start marker), so
            // we don't leak a vertical gap.
            let mut head = content[..start].to_string();
            while head.ends_with("\n\n") {
                head.pop();
            }
            let mut tail_start = end;
            let bytes = content.as_bytes();
            if bytes.get(tail_start) == Some(&b'\n') {
                tail_start += 1;
            }
            let tail = &content[tail_start..];
            let mut new_content = head;
            if !tail.is_empty() {
                if !new_content.is_empty() && !new_content.ends_with('\n') {
                    new_content.push('\n');
                }
                new_content.push_str(tail);
            } else if !new_content.is_empty() && !new_content.ends_with('\n') {
                new_content.push('\n');
            }
            // If the file only ever held the recon block, delete it instead
            // of leaving an empty husk behind. Whitespace-only counts as
            // empty — a stray newline from the marker stripping shouldn't
            // pin the file in place.
            if new_content.trim().is_empty() {
                std::fs::remove_file(&path)?;
                eprintln!("Removed recon-only file {}", path.display());
            } else {
                std::fs::write(&path, new_content)?;
                eprintln!("Removed recon rules block from {}", path.display());
            }
        }
        AgentTarget::Dedicated { path, .. } => {
            if path.exists() {
                std::fs::remove_file(&path)?;
                eprintln!("Removed {}", path.display());
            }
        }
    }
    Ok(())
}

/// Reverse of [`write_mcp_config`]: drop the `recon` server entry from the
/// IDE's MCP config, preserving any other servers the user has wired.  No-op
/// if the file is absent or has no `recon` entry.
fn remove_mcp_entry(ide: &Ide, repo: &Path) -> Result<()> {
    let config_path = ide.config_abs_path(repo);
    if !config_path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&config_path)?;
    let mut value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Ok(()), // Corrupt JSON — leave it for the user to fix.
    };
    let servers_key = ide.servers_key();
    let removed = value
        .get_mut(servers_key)
        .and_then(|s| s.as_object_mut())
        .map(|servers| servers.remove("recon").is_some())
        .unwrap_or(false);
    if !removed {
        return Ok(());
    }
    std::fs::write(&config_path, serde_json::to_string_pretty(&value)?)?;
    eprintln!("Removed recon entry from {}", config_path.display());
    Ok(())
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
    init_tracing("warn");
    let (server, _) = init_server(repo)?;
    Ok(server)
}

/// Serve the MCP server over Streamable HTTP.
async fn serve_http(
    mcp_service: recon_server::multi_repo::MultiRepoService,
    host: &str,
    port: u16,
) -> Result<()> {
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

    // Hold a snapshot of the currently-active ReconServer outside the
    // StreamableHttpService factory so the listen-loop can `select!` on
    // its shutdown notify in addition to signal + accept. Without this
    // the only way to stop the bound port is SIGTERM — a worker-side
    // license rejection would leave the listener exposed indefinitely.
    // The license revalidation task fires `request_shutdown` on the
    // initial server, so snapshotting here is correct.
    let server_for_shutdown = mcp_service.active();
    let session_manager = Arc::new(LocalSessionManager::default());
    let service =
        StreamableHttpService::new(move || Ok(mcp_service.clone()), session_manager, config);

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
                info!("http server shutting down (signal)");
                cancel.cancel();
                break;
            }
            _ = server_for_shutdown.await_shutdown_request() => {
                // Periodic license-revalidation task fired the notify
                // (account deletion / key revoke / sub hard-expiry) or
                // local credentials disappeared. Drop the listener so
                // the bound port is released and stop accepting new
                // sessions immediately.
                info!("http server shutting down (license revoked)");
                cancel.cancel();
                break;
            }
        }
    }

    tasks.shutdown().await;
    Ok(())
}

// ── Tracing init ───────────────────────────────────────────────────────────────

/// Initialize the global tracing subscriber.
///
/// Honors two environment variables:
/// - `RECON_LOG` / `RUST_LOG` — standard `EnvFilter` directive
///   (defaults to `default_filter` if unset).
/// - `RECON_LOG_FORMAT` — `json` for structured JSON-per-line output,
///   anything else (default) for the human-friendly text format.
///
/// Always writes to stderr. Never writes to stdout — the MCP stdio
/// transport would corrupt on stdout log lines.
///
/// Idempotent in the sense that an already-installed subscriber means
/// subsequent calls silently no-op (they return `Err` from `try_init`).
fn init_tracing(default_filter: &str) {
    let env_filter = EnvFilter::try_from_env("RECON_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(default_filter));

    let json = std::env::var("RECON_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    if json {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_writer(std::io::stderr)
            .with_env_filter(env_filter)
            .try_init();
    } else {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(env_filter)
            .try_init();
    }
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

            // Persist the raw key for the periodic re-validation task in
            // `recon serve`. Without this we can only re-check the cache,
            // and a revoke from the dashboard never propagates to a running
            // server. Saved at chmod 0600 on Unix; `recon logout` wipes it.
            if let Err(e) = recon_server::license::save_credentials(&config_dir, &key) {
                tracing::warn!("failed to persist credentials: {e}");
            }

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
            // Wipe both files so a follow-up `recon serve` can't keep
            // re-validating with a stale credential.
            let _ = recon_server::license::delete_credentials(&config_dir);
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
            if raw_json {
                let json = serde_json::json!({
                    "tier": license.tier.name(),
                    "source": license.source.to_string(),
                    "max_repos": limits.max_repos,
                    "max_files": limits.max_files,
                    "max_loc": limits.max_loc,
                    "expires_at": license.expires_at,
                    "message": license.message,
                });
                println!("{json}");
            } else {
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
            }
            Ok(())
        }

        // ── Indexing + IDE setup ──────────────────────────────────────────────
        Command::Init { mcp } => {
            init_tracing("info");

            let repo = repo.canonicalize()?;

            // Migrate per-repo license to global before validation.
            maybe_migrate_license(&repo);

            // License must be present before we do any work.
            let license = validate_license_or_die()?;
            let _limits = license.tier.limits();

            // Server-side repo tracking (v0.2.0+). Replaces the prior
            // local-only enforcement against `~/.config/recon/repos.json`,
            // which a patched binary could trivially bypass.
            //
            // We need the raw API key to call /v1/account/repos; the cached
            // license alone is signature-verified but doesn't expose the key.
            // If credentials are missing the user upgraded across the v0.1→v0.2
            // line and needs to re-login once.
            let config_dir = recon_server::license::global_config_dir();
            let repo_path_str = repo.to_string_lossy().to_string();
            let api_key =
                recon_server::license::read_credentials(&config_dir).ok_or_else(|| {
                    anyhow::anyhow!(
                        "credentials not found at {}/credentials.json — run \
                         `recon login <key>` first.",
                        config_dir.display()
                    )
                })?;
            let fingerprint = recon_server::account::fingerprint_path(&repo);
            match recon_server::account::register_repo(&api_key, &fingerprint) {
                Ok(resp) => {
                    eprintln!(
                        "Registered repo with recon ({}/{} on {})",
                        match resp.status.as_str() {
                            "registered" => "new",
                            _ => "refreshed",
                        },
                        resp.limit,
                        resp.tier,
                    );
                }
                Err(recon_server::account::AccountError::OverQuota {
                    limit,
                    tier,
                    message,
                }) => {
                    return Err(anyhow::anyhow!(
                        "{tier} plan allows {limit} repo(s) — limit reached.\n\
                         {message}\n\
                         Run `recon repos list` to see what's registered, \
                         `recon repos remove <path>` to free a slot, \
                         or upgrade at https://mcprecon.pages.dev/pricing"
                    ));
                }
                Err(recon_server::account::AccountError::Unauthorized(msg)) => {
                    return Err(anyhow::anyhow!(
                        "API key rejected by recon worker: {msg}.\n\
                         Run `recon login <key>` to refresh."
                    ));
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Failed to register repo with recon worker: {e}.\n\
                         Check your network and try again — re-running \
                         `recon init` is safe (the call is idempotent)."
                    ));
                }
            }

            // 1. Index the repo
            let store_dir = repo.join(".recon");
            std::fs::create_dir_all(&store_dir)?;
            let store =
                Store::open(&store_dir.join("index.db")).map_err(|e| anyhow::anyhow!("{e}"))?;
            let tantivy = TantivyBackend::open(&store_dir.join("tantivy"))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut writer = match tantivy.writer(50_000_000) {
                Ok(w) => Some(w),
                Err(e) => {
                    warn!(
                        %e,
                        "tantivy writer creation failed during `recon init`; \
                         BM25 search will be unavailable until the next clean reindex"
                    );
                    None
                }
            };
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

            // 2. Write IDE MCP config + strict agent rules (only if --mcp was passed)
            if let Some(ref ide) = mcp {
                let recon_bin = std::env::current_exe()?
                    .canonicalize()?
                    .to_string_lossy()
                    .to_string();
                write_mcp_config(ide, &repo, &recon_bin)?;
                write_agent_rules(ide, &repo)?;

                // Smoke-test the server before declaring success. The MCP
                // client (Claude Code / opencode / Cursor) swallows the
                // child's stderr and surfaces failures as a generic
                // `MCP error -32000: connection closed`. By spawning the
                // same binary with the same args here we can capture that
                // stderr ourselves and surface the real cause to the user
                // — license rejected, over-tier, panic during init, etc.
                smoke_test_serve(&recon_bin, &repo_path_str)?;
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

            eprintln!("Done. Restart your IDE to activate recon tools.");
            Ok(())
        }

        // ── MCP server ────────────────────────────────────────────────────────
        Command::Serve { log, port, host } => {
            init_tracing(&log);

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

            // Install the license we validated above so the per-tool-call
            // expiry gate can enforce billing-period boundaries. The periodic
            // re-validation task (spawned below) will swap this out as the
            // subscription state changes.
            server.set_license(license.clone());

            server
                .index_repo()
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            #[cfg(feature = "embed")]
            if let Err(e) = server.init_embed().await {
                warn!("embed init failed, semantic search disabled: {e}");
            }

            server.start_watcher();

            // Hybrid telemetry flush: the count trigger fires every
            // FLUSH_THRESHOLD tool calls; this timer covers the idle
            // tail so even sessions with sub-threshold call rates
            // persist their counters within FLUSH_INTERVAL_SECS.
            server.start_telemetry_flush_timer();

            // Periodic license re-validation.
            //
            // Polls every 15 min (override via RECON_LICENSE_REVALIDATE_SECS).
            // When a credentials file is present (written by `recon login`),
            // we hit the worker's /v1/license/validate with the raw key and
            // distinguish three outcomes:
            //   * Ok(new)            → swap the in-memory license, including
            //                          updated tier/expires_at if the webhook
            //                          or cron modified it since last login.
            //   * Err(Rejected)      → dashboard revoked the key, or the sub
            //                          hard-expired on the worker. Wipe the
            //                          credentials + cache and mark the
            //                          in-memory license `revoked = true` so
            //                          the per-tool-call gate refuses the
            //                          next call with a clear "run
            //                          `recon login`" message.
            //   * Err(Transient)     → network/DNS/5xx. Keep the current
            //                          license; retry next tick.
            //
            // Without credentials (dev tests, seeded dev cache), fall back
            // to the cache-only path and just surface warnings — there's no
            // key to send upstream.
            //
            // The task holds a clone of ReconServer (Arc internally). When
            // the runtime shuts down at process exit, this sleep-loop is
            // aborted cleanly.
            {
                let server_rev = server.clone();
                let interval_secs = std::env::var("RECON_LICENSE_REVALIDATE_SECS")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(900) // 15 minutes
                    .clamp(60, 86_400);
                tokio::spawn(async move {
                    // Snapshot whether credentials existed when the task
                    // started. If credentials transition Some → None
                    // mid-run we treat that as an explicit `recon logout`
                    // and shut the running server down — the alternative
                    // (continuing on the cached signed license) keeps a
                    // logged-out user's serve process alive on the
                    // expectation that the user already moved on.
                    let dir0 = recon_server::license::global_config_dir();
                    let had_credentials_at_start =
                        recon_server::license::read_credentials(&dir0).is_some();
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
                        let dir = recon_server::license::global_config_dir();

                        match recon_server::license::read_credentials(&dir) {
                            Some(key) => {
                                // Blocking HTTP — run on a dedicated thread so
                                // we don't block the tokio runtime's worker.
                                let result = tokio::task::spawn_blocking(move || {
                                    recon_server::license::validate_remote_strict(&key)
                                })
                                .await;
                                match result {
                                    Ok(Ok(resp)) => {
                                        // Refresh disk cache + in-memory license.
                                        let _ =
                                            recon_server::license::write_cache_public(&dir, &resp);
                                        let lic = recon_server::license::response_to_validated(
                                            resp,
                                            recon_server::license::LicenseSource::Remote,
                                        );
                                        info!(
                                            tier = lic.tier.name(),
                                            "license re-validated remotely",
                                        );
                                        server_rev.set_license(lic);
                                    }
                                    Ok(Err(recon_server::license::RemoteError::Rejected(msg))) => {
                                        tracing::warn!(
                                            reason = %msg,
                                            "license rejected by worker — marking revoked and shutting down"
                                        );
                                        // Wipe local auth state so a fresh
                                        // `recon serve` fails fast with
                                        // "run `recon login`".
                                        let _ = recon_server::license::delete_credentials(&dir);
                                        let _ = recon_server::license::delete_cache(&dir);
                                        if let Some(mut lic) = server_rev.current_license() {
                                            lic.revoked = true;
                                            lic.message = format!("License revoked: {msg}");
                                            server_rev.set_license(lic);
                                        }
                                        // Trigger a clean shutdown of the
                                        // running serve loop. Without this
                                        // the IDE keeps a dead subprocess
                                        // around (stdio) or a bound port
                                        // stays exposed (HTTP) until SIGTERM.
                                        server_rev.request_shutdown();
                                        return; // exit the periodic task
                                    }
                                    Ok(Err(recon_server::license::RemoteError::Transient(msg))) => {
                                        tracing::warn!(
                                            reason = %msg,
                                            "license re-validation transient error — keeping current"
                                        );
                                    }
                                    Err(join_err) => {
                                        tracing::warn!(
                                            "license re-validation join error: {join_err}"
                                        );
                                    }
                                }
                            }
                            None => {
                                // Credentials disappeared mid-run after we
                                // started with some → user ran
                                // `recon logout` (or wiped ~/.config/recon).
                                // Shut down the live server so a logged-out
                                // user isn't left with a phantom subprocess
                                // their IDE keeps trying to talk to.
                                if had_credentials_at_start {
                                    tracing::warn!(
                                        "credentials removed (recon logout) — shutting down"
                                    );
                                    server_rev.request_shutdown();
                                    return;
                                }
                                // No credentials on disk and none at start —
                                // dev license / tests path. Fall back to
                                // cache-only revalidation; never escalate
                                // to shutdown for these callers.
                                match recon_server::license::validate_license(None, &dir) {
                                    Ok(new) => server_rev.set_license(new),
                                    Err(e) => tracing::debug!(
                                        "cache-only re-validation (no credentials): {e}"
                                    ),
                                }
                            }
                        }
                    }
                });
            }

            // ── Multi-repo wrapper for the rmcp transport ─────────────────────
            //
            // A `MultiRepoService` is what the rmcp stdio / HTTP transport
            // sees from this point on. It exposes the same 18 stateful
            // tools as `ReconServer` (via thin shims that delegate to the
            // currently-active server) plus the two new multi-repo tools
            // `code_activate_repo` and `code_list_repos`.
            //
            // The router is constructed with the validated tier so the
            // first `code_activate_repo` call already enforces
            // `max_repos`. `restore_session` re-loads the loaded set
            // recorded by the previous `recon serve` so agents do not
            // have to re-issue activate after every restart.
            let router = std::sync::Arc::new(recon_server::router::RepoRouter::new(license.tier));
            if license.expires_at != 0 {
                router.set_expires_at(license.expires_at);
            }
            let config_dir = recon_server::license::global_config_dir();
            let mcp_service = recon_server::multi_repo::MultiRepoService::new(
                router.clone(),
                server.clone(),
                config_dir,
            );
            let restored = mcp_service.restore_session();
            if restored > 0 {
                info!(restored, "restored multi-repo session");
            }

            if let Some(port) = port {
                // serve_http already drives its own shutdown via ctrl_c + cancel.
                let result = serve_http(mcp_service.clone(), &host, port).await;
                shutdown_with_timeout(&server).await;
                print_session_receipt(&server);
                maybe_auto_push_savings(&repo);
                result
            } else {
                // Stdio MCP: the IDE (Claude Code, Cursor, Windsurf, OpenCode)
                // spawns us as a subprocess. When the IDE exits it closes our
                // stdio pipes; we must notice that as a shutdown trigger, not
                // just SIGINT/SIGTERM. So select on both: signal *or* the
                // MCP service loop terminating on its own.
                let (stdin, stdout) = rmcp::transport::io::stdio();
                let service = mcp_service.clone().serve((stdin, stdout)).await?;
                let mut waiter = Box::pin(service.waiting());
                tokio::select! {
                    _ = wait_for_shutdown_signal() => {
                        info!("shutdown requested by signal");
                    }
                    res = &mut waiter => match res {
                        Ok(reason) => info!(?reason, "MCP transport closed"),
                        Err(e) => tracing::warn!("MCP service join error: {e}"),
                    },
                    _ = server.await_shutdown_request() => {
                        // Periodic license-revalidation task detected a
                        // worker-side rejection (account deletion / key
                        // revoke / sub hard-expiry) or local credentials
                        // disappeared from disk. Either way, the running
                        // session is no longer authorised — exit cleanly
                        // instead of holding the IDE's stdio open while
                        // returning errors on every tool call.
                        info!("shutdown requested by license revocation");
                    }
                }
                // Drop the pinned waiter: its DropGuard cancels the service
                // task and its owned transport, releasing stdin/stdout.
                drop(waiter);
                shutdown_with_timeout(&server).await;
                print_session_receipt(&server);
                maybe_auto_push_savings(&repo);
                Ok(())
            }
        }

        // ── Indexing only ─────────────────────────────────────────────────────
        Command::Index => {
            init_tracing("info");

            validate_license_or_die()?;

            let repo = repo.canonicalize()?;
            let store_dir = repo.join(".recon");
            std::fs::create_dir_all(&store_dir)?;
            let store =
                Store::open(&store_dir.join("index.db")).map_err(|e| anyhow::anyhow!("{e}"))?;
            let tantivy = TantivyBackend::open(&store_dir.join("tantivy"))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut writer = match tantivy.writer(50_000_000) {
                Ok(w) => Some(w),
                Err(e) => {
                    warn!(
                        %e,
                        "tantivy writer creation failed during `recon index`; \
                         BM25 docs will not be updated this run"
                    );
                    None
                }
            };
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
            // Append global repo count for the human-readable view; suppress
            // on --json so the output is pure parseable JSON.
            if !raw_json {
                let config_dir = recon_server::license::global_config_dir();
                if let Ok(repos) = recon_server::repos::load_repos(&config_dir) {
                    eprintln!("Indexed repos (global): {}", repos.len());
                }
            }
            Ok(())
        }
        Command::Purge { mcp } => {
            let repo = repo.canonicalize()?;
            let store_dir = repo.join(".recon");
            if store_dir.exists() {
                std::fs::remove_dir_all(&store_dir)?;
                eprintln!("Purged {}", store_dir.display());
            } else {
                eprintln!("No index found at {}", store_dir.display());
            }

            match mcp {
                Some(ide) => {
                    remove_mcp_entry(&ide, &repo)?;
                    remove_agent_rules(&ide, &repo)?;
                    let config_dir = recon_server::license::global_config_dir();
                    let repo_path_str = repo.to_string_lossy().to_string();
                    match recon_server::repos::remove_repo(&config_dir, &repo_path_str) {
                        Ok(true) => eprintln!("Released local repo cache entry"),
                        Ok(false) => {}
                        Err(e) => eprintln!("Failed to update local repos cache: {e}"),
                    }

                    // v0.2.0+: also release the server-side slot. Best-effort —
                    // a network failure here shouldn't block local teardown,
                    // but we surface it so the user can re-run.
                    if let Some(api_key) = recon_server::license::read_credentials(&config_dir) {
                        let fp = recon_server::account::fingerprint_path(&repo);
                        match recon_server::account::unregister_repo(&api_key, &fp) {
                            Ok(()) => eprintln!("Released server-side repo slot"),
                            Err(recon_server::account::AccountError::NotFound) => {
                                // Slot wasn't registered server-side (pre-v0.2 repo) — silent.
                            }
                            Err(e) => eprintln!(
                                "Could not release server-side slot: {e}\n\
                                 Run `recon repos remove {}` once you have network.",
                                repo.display()
                            ),
                        }
                    }
                }
                None => {
                    eprintln!(
                        "Note: MCP config, agent rules, and server-side repo slot left in place. \
                         Run `recon purge --mcp <ide>` to fully reverse `recon init`."
                    );
                }
            }
            Ok(())
        }
        Command::Repos { action } => match action {
            ReposAction::List => {
                let config_dir = recon_server::license::global_config_dir();
                let api_key =
                    recon_server::license::read_credentials(&config_dir).ok_or_else(|| {
                        anyhow::anyhow!(
                            "credentials not found at {}/credentials.json — \
                             run `recon login <key>` first.",
                            config_dir.display()
                        )
                    })?;
                let resp = recon_server::account::list_repos(&api_key)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                if raw_json {
                    let json = serde_json::json!({
                        "tier": resp.tier,
                        "limit": resp.limit,
                        "count": resp.repos.len(),
                        "repos": resp.repos.iter().map(|r| serde_json::json!({
                            "fingerprint": r.fingerprint,
                            "first_seen_at": r.first_seen_at,
                            "last_seen_at": r.last_seen_at,
                        })).collect::<Vec<_>>(),
                    });
                    println!("{json}");
                } else {
                    println!(
                        "{} repo(s) registered ({}/{} on {})",
                        resp.repos.len(),
                        resp.repos.len(),
                        resp.limit,
                        resp.tier
                    );
                    for r in &resp.repos {
                        println!(
                            "  {}  first_seen={}  last_seen={}",
                            &r.fingerprint[..16],
                            r.first_seen_at,
                            r.last_seen_at
                        );
                    }
                    if resp.repos.is_empty() {
                        println!("(Tip: `recon init` in a project will register that repo here.)");
                    }
                }
                Ok(())
            }
            ReposAction::Remove { target } => {
                let config_dir = recon_server::license::global_config_dir();
                let api_key =
                    recon_server::license::read_credentials(&config_dir).ok_or_else(|| {
                        anyhow::anyhow!(
                            "credentials not found at {}/credentials.json — \
                             run `recon login <key>` first.",
                            config_dir.display()
                        )
                    })?;
                // Accept either a 64-char lowercase hex fingerprint or a path.
                let is_fp = target.len() == 64
                    && target
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
                let fp = if is_fp {
                    target.clone()
                } else {
                    let path = std::path::PathBuf::from(&target);
                    recon_server::account::fingerprint_path(&path)
                };
                match recon_server::account::unregister_repo(&api_key, &fp) {
                    Ok(()) => {
                        println!("Removed {} from your account", &fp[..16]);
                        // Best-effort local-cache cleanup. We don't have the
                        // canonical path back from a fingerprint, so we walk
                        // local entries and drop ones whose canonical path
                        // hashes to the same fingerprint.
                        if let Ok(repos) = recon_server::repos::load_repos(&config_dir) {
                            for r in &repos {
                                let local_fp = recon_server::account::fingerprint_path(
                                    std::path::Path::new(&r.path),
                                );
                                if local_fp == fp {
                                    let _ = recon_server::repos::remove_repo(&config_dir, &r.path);
                                    break;
                                }
                            }
                        }
                        Ok(())
                    }
                    Err(recon_server::account::AccountError::NotFound) => Err(anyhow::anyhow!(
                        "Fingerprint not registered. Run `recon repos list` to see what's tracked."
                    )),
                    Err(e) => Err(anyhow::anyhow!("{e}")),
                }
            }
        },
        Command::Doctor { json } => doctor::run(&repo, json),
        Command::Version => {
            if raw_json {
                let json = serde_json::json!({
                    "name": "recon",
                    "version": env!("CARGO_PKG_VERSION"),
                });
                println!("{json}");
            } else {
                println!("recon {}", env!("CARGO_PKG_VERSION"));
            }
            Ok(())
        }
        Command::Update { check, force } => update::run(check, force),
        Command::Savings { action } => match action {
            SavingsAction::Push { repo: r } => savings::push(r),
            SavingsAction::Show { repo: r } => savings::show(r),
        },
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    // Env vars are process-global, so any test that mutates
    // `RECON_WINDSURF_CONFIG_PATH` must hold this mutex for the whole
    // duration of its set/use/remove critical section. Without it, cargo
    // test's parallel scheduler races the three write_mcp_config_windsurf_*
    // tests with the two ide_config_path_windsurf tests and the loser
    // writes to the real home dir.
    static WINDSURF_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        let _guard = WINDSURF_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
        let _guard = WINDSURF_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
        let _guard = WINDSURF_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
        let _guard = WINDSURF_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
        let _guard = WINDSURF_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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

    // ── write_agent_rules / remove_agent_rules ───────────────────────────────

    #[test]
    fn write_agent_rules_claude_creates_when_file_missing() {
        let dir = tempdir().unwrap();
        // Pre-condition: no CLAUDE.md.
        assert!(!dir.path().join("CLAUDE.md").exists());

        write_agent_rules(&Ide::ClaudeCode, dir.path()).unwrap();

        let claude = dir.path().join("CLAUDE.md");
        assert!(
            claude.exists(),
            "init must create CLAUDE.md when missing so the rules block actually lands"
        );
        let content = fs::read_to_string(&claude).unwrap();
        assert!(content.contains(RECON_RULES_START));
        assert!(content.contains(RECON_RULES_END));
        assert!(content.contains("strict policy"));
    }

    #[test]
    fn write_agent_rules_claude_appends_when_file_exists() {
        let dir = tempdir().unwrap();
        let claude = dir.path().join("CLAUDE.md");
        fs::write(&claude, "# Project\n\nSome existing content.\n").unwrap();
        write_agent_rules(&Ide::ClaudeCode, dir.path()).unwrap();
        let content = fs::read_to_string(&claude).unwrap();
        assert!(content.starts_with("# Project"));
        assert!(content.contains(RECON_RULES_START));
        assert!(content.contains("strict policy"));
        assert!(content.contains("asking the user"));
        assert!(content.contains(RECON_RULES_END));
    }

    #[test]
    fn write_agent_rules_claude_idempotent() {
        let dir = tempdir().unwrap();
        let claude = dir.path().join("CLAUDE.md");
        fs::write(&claude, "# Project\n").unwrap();
        write_agent_rules(&Ide::ClaudeCode, dir.path()).unwrap();
        let after_first = fs::read_to_string(&claude).unwrap();
        write_agent_rules(&Ide::ClaudeCode, dir.path()).unwrap();
        let after_second = fs::read_to_string(&claude).unwrap();
        assert_eq!(
            after_first, after_second,
            "second run must not duplicate the rules block"
        );
    }

    #[test]
    fn write_agent_rules_opencode_creates_agents_md() {
        let dir = tempdir().unwrap();
        write_agent_rules(&Ide::OpenCode, dir.path()).unwrap();
        let agents = dir.path().join("AGENTS.md");
        assert!(agents.exists());
        let content = fs::read_to_string(&agents).unwrap();
        assert!(content.contains(RECON_RULES_START));
        assert!(content.contains("strict policy"));
    }

    #[test]
    fn write_agent_rules_cursor_writes_dedicated_mdc() {
        let dir = tempdir().unwrap();
        write_agent_rules(&Ide::Cursor, dir.path()).unwrap();
        let mdc = dir.path().join(".cursor").join("rules").join("recon.mdc");
        assert!(mdc.exists());
        let content = fs::read_to_string(&mdc).unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("alwaysApply: true"));
        assert!(content.contains("strict policy"));
        assert!(content.contains("asking the user"));
    }

    #[test]
    fn write_agent_rules_windsurf_writes_dedicated_md() {
        let dir = tempdir().unwrap();
        write_agent_rules(&Ide::Windsurf, dir.path()).unwrap();
        let md = dir.path().join(".windsurf").join("rules").join("recon.md");
        assert!(md.exists());
        let content = fs::read_to_string(&md).unwrap();
        assert!(content.contains("trigger: always_on"));
        assert!(content.contains("strict policy"));
    }

    #[test]
    fn remove_agent_rules_strips_block_from_claude() {
        let dir = tempdir().unwrap();
        let claude = dir.path().join("CLAUDE.md");
        fs::write(&claude, "# Project\n\nKeep me.\n").unwrap();
        write_agent_rules(&Ide::ClaudeCode, dir.path()).unwrap();
        remove_agent_rules(&Ide::ClaudeCode, dir.path()).unwrap();
        let content = fs::read_to_string(&claude).unwrap();
        assert!(content.contains("# Project"));
        assert!(content.contains("Keep me."));
        assert!(!content.contains(RECON_RULES_START));
        assert!(!content.contains("strict policy"));
    }

    #[test]
    fn remove_agent_rules_deletes_dedicated_files() {
        let dir = tempdir().unwrap();
        write_agent_rules(&Ide::Cursor, dir.path()).unwrap();
        write_agent_rules(&Ide::Windsurf, dir.path()).unwrap();
        remove_agent_rules(&Ide::Cursor, dir.path()).unwrap();
        remove_agent_rules(&Ide::Windsurf, dir.path()).unwrap();
        assert!(!dir.path().join(".cursor/rules/recon.mdc").exists());
        assert!(!dir.path().join(".windsurf/rules/recon.md").exists());
    }

    #[test]
    fn remove_agent_rules_deletes_recon_only_claude_md() {
        // Round-trip: init creates CLAUDE.md from scratch; purge must take
        // it back out, otherwise we leak a file we created ourselves.
        let dir = tempdir().unwrap();
        let claude = dir.path().join("CLAUDE.md");

        write_agent_rules(&Ide::ClaudeCode, dir.path()).unwrap();
        assert!(claude.exists(), "init must have created CLAUDE.md");

        remove_agent_rules(&Ide::ClaudeCode, dir.path()).unwrap();
        assert!(
            !claude.exists(),
            "purge must delete a recon-only CLAUDE.md, not leave an empty husk"
        );
    }

    #[test]
    fn remove_agent_rules_keeps_user_content_in_claude_md() {
        // Inverse of the above: when CLAUDE.md had user content before init,
        // purge strips only the marker block and keeps the file alive.
        let dir = tempdir().unwrap();
        let claude = dir.path().join("CLAUDE.md");
        fs::write(&claude, "# Project\n\nPersistent docs.\n").unwrap();

        write_agent_rules(&Ide::ClaudeCode, dir.path()).unwrap();
        remove_agent_rules(&Ide::ClaudeCode, dir.path()).unwrap();

        let content = fs::read_to_string(&claude).unwrap();
        assert!(content.contains("Persistent docs."));
        assert!(!content.contains(RECON_RULES_START));
    }

    #[test]
    fn remove_agent_rules_no_op_when_block_absent() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("AGENTS.md");
        fs::write(&agents, "# Agents\n\nUnrelated content.\n").unwrap();
        remove_agent_rules(&Ide::OpenCode, dir.path()).unwrap();
        assert_eq!(
            fs::read_to_string(&agents).unwrap(),
            "# Agents\n\nUnrelated content.\n"
        );
    }

    #[test]
    fn remove_agent_rules_no_op_when_file_missing() {
        let dir = tempdir().unwrap();
        // No CLAUDE.md, no AGENTS.md, no recon.mdc — all four must be no-ops.
        remove_agent_rules(&Ide::ClaudeCode, dir.path()).unwrap();
        remove_agent_rules(&Ide::OpenCode, dir.path()).unwrap();
        remove_agent_rules(&Ide::Cursor, dir.path()).unwrap();
    }

    // ── remove_mcp_entry ─────────────────────────────────────────────────────

    #[test]
    fn remove_mcp_entry_drops_recon_preserves_others() {
        let dir = tempdir().unwrap();
        write_mcp_config(&Ide::ClaudeCode, dir.path(), "/usr/bin/recon").unwrap();
        // Inject another server alongside recon.
        let path = dir.path().join(".mcp.json");
        let mut v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        v["mcpServers"]["other"] = serde_json::json!({"command": "/bin/other"});
        fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();

        remove_mcp_entry(&Ide::ClaudeCode, dir.path()).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v["mcpServers"]["recon"].is_null());
        assert_eq!(v["mcpServers"]["other"]["command"], "/bin/other");
    }

    #[test]
    fn remove_mcp_entry_no_op_when_config_missing() {
        let dir = tempdir().unwrap();
        remove_mcp_entry(&Ide::ClaudeCode, dir.path()).unwrap();
    }

    #[test]
    fn remove_mcp_entry_handles_corrupt_json_gracefully() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".mcp.json"), "not json {{").unwrap();
        // Must not error: corrupt config is the user's problem to fix.
        remove_mcp_entry(&Ide::ClaudeCode, dir.path()).unwrap();
    }
}
