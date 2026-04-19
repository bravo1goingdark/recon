mod pretty;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::sync::atomic::{AtomicU64, Ordering};

static REQ_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Parser)]
#[command(name = "recon", about = "Query a remote recon MCP server")]
struct Cli {
    /// Server URL (or set RECON_URL env)
    #[arg(
        long,
        global = true,
        env = "RECON_URL",
        default_value = "http://127.0.0.1:3100"
    )]
    server: String,

    /// Output raw JSON instead of formatted text
    #[arg(long, global = true)]
    json: bool,

    /// MCP session ID (reused across calls)
    #[arg(long, global = true, env = "RECON_SESSION")]
    session: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
        /// Filter DSL
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
    /// Show index health stats
    Stats,
    /// Force full re-index
    Reindex,
    /// Connect and show server info
    Ping,
}

/// Send a JSON-RPC tools/call to the server and return the text content.
async fn call_tool(
    client: &reqwest::Client,
    server: &str,
    session: Option<&str>,
    tool: &str,
    args: serde_json::Value,
) -> Result<String> {
    let id = REQ_ID.fetch_add(1, Ordering::Relaxed);
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": args,
        }
    });

    let mut req = client
        .post(server)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");

    if let Some(sid) = session {
        req = req.header("Mcp-Session-Id", sid);
    }

    let resp = req.json(&body).send().await?;
    let text = resp.text().await?;

    // Parse SSE: find "data: {..." lines
    for line in text.lines() {
        let line = line.trim();
        if let Some(data) = line.strip_prefix("data: ") {
            if let Ok(msg) = serde_json::from_str::<serde_json::Value>(data) {
                // Extract tool result text from content array
                if let Some(content) = msg
                    .get("result")
                    .and_then(|r| r.as_object())
                    .and_then(|r| r.get("content"))
                    .and_then(|c| c.as_array())
                {
                    if let Some(text_val) = content
                        .first()
                        .and_then(|c| c.get("text"))
                        .and_then(|t| t.as_str())
                    {
                        return Ok(text_val.to_string());
                    }
                }
                // Might be an error
                if let Some(err) = msg.get("error") {
                    return Ok(format!("Error: {}", err));
                }
            }
        }
    }

    // Fallback: try parsing the whole response as JSON-RPC
    if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) {
        if let Some(text_val) = msg
            .get("result")
            .and_then(|r| r.as_object())
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
        {
            return Ok(text_val.to_string());
        }
    }

    Ok(text)
}

/// Send initialize + initialized to establish a session, return session ID.
async fn init_session(client: &reqwest::Client, server: &str) -> Result<Option<String>> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "recon-cli", "version": env!("CARGO_PKG_VERSION") }
        }
    });

    let resp = client
        .post(server)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await?;

    let session_id = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let text = resp.text().await?;

    // Send initialized notification
    if let Some(ref sid) = session_id {
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let _ = client
            .post(server)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Mcp-Session-Id", sid.as_str())
            .json(&notif)
            .send()
            .await;
    }

    // Parse server info for ping
    for line in text.lines() {
        if let Some(data) = line.trim().strip_prefix("data: ") {
            if let Ok(msg) = serde_json::from_str::<serde_json::Value>(data) {
                if let Some(info) = msg.get("result").and_then(|r| r.get("serverInfo")) {
                    let name = info.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                    let ver = info.get("version").and_then(|v| v.as_str()).unwrap_or("?");
                    eprintln!("Connected to {name} {ver}");
                }
            }
        }
    }

    Ok(session_id)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let raw_json = cli.json;
    let out = |s: &str| pretty::print_output(s, raw_json);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // Get or create session
    let session = match &cli.session {
        Some(s) => Some(s.clone()),
        None => init_session(&client, &cli.server).await?,
    };
    let session_ref = session.as_deref();

    match cli.command {
        Command::Ping => {
            // Session already initialized above, info printed
            if let Some(sid) = &session {
                eprintln!("Session: {sid}");
            }
            Ok(())
        }
        Command::Find { name, kind, lang } => {
            let args = serde_json::json!({ "name": name, "kind": kind, "lang": lang });
            out(&call_tool(&client, &cli.server, session_ref, "code_find_symbol", args).await?);
            Ok(())
        }
        Command::Search {
            query,
            mode,
            filter,
        } => {
            let args = serde_json::json!({ "query": query, "mode": mode, "filter": filter });
            out(&call_tool(&client, &cli.server, session_ref, "code_search", args).await?);
            Ok(())
        }
        Command::Outline { path } => {
            let args = serde_json::json!({ "path": path });
            out(&call_tool(&client, &cli.server, session_ref, "code_outline", args).await?);
            Ok(())
        }
        Command::Skeleton { path, depth } => {
            let args = serde_json::json!({ "path": path, "depth": depth });
            out(&call_tool(&client, &cli.server, session_ref, "code_skeleton", args).await?);
            Ok(())
        }
        Command::Symbol { path, name } => {
            let args = serde_json::json!({ "path": path, "symbol_or_line": name });
            out(&call_tool(&client, &cli.server, session_ref, "code_read_symbol", args).await?);
            Ok(())
        }
        Command::Refs { symbol } => {
            let args = serde_json::json!({ "symbol": symbol });
            out(&call_tool(&client, &cli.server, session_ref, "code_find_refs", args).await?);
            Ok(())
        }
        Command::Ls { glob, lang, filter } => {
            let args = serde_json::json!({ "glob": glob, "lang": lang, "filter": filter });
            out(&call_tool(&client, &cli.server, session_ref, "code_list", args).await?);
            Ok(())
        }
        Command::Map { budget, focus } => {
            let focus = if focus.is_empty() { None } else { Some(focus) };
            let args = serde_json::json!({ "focus_files": focus, "token_budget": budget });
            out(&call_tool(&client, &cli.server, session_ref, "code_repo_map", args).await?);
            Ok(())
        }
        Command::Stats => {
            out(&call_tool(
                &client,
                &cli.server,
                session_ref,
                "code_stats",
                serde_json::json!({}),
            )
            .await?);
            Ok(())
        }
        Command::Reindex => {
            out(&call_tool(
                &client,
                &cli.server,
                session_ref,
                "code_reindex",
                serde_json::json!({}),
            )
            .await?);
            Ok(())
        }
    }
}
