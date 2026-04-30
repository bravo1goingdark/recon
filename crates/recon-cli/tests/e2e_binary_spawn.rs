//! End-to-end binary spawn verifier.
//!
//! This is the automated equivalent of the user flow the product ships:
//!   1. User runs `recon login <key>` → license cached in global config dir.
//!   2. User runs `recon init --mcp cc` → writes .mcp.json pointing at the
//!      binary.
//!   3. IDE (Claude Code / Cursor / Windsurf / OpenCode) spawns the binary
//!      with pipes over stdin/stdout.
//!   4. IDE speaks MCP JSON-RPC: initialize → initialized → tools/call.
//!   5. User closes the IDE → pipes close → server drains and exits.
//!
//! The existing e2e_full_pipeline tests call `ReconServer::new` directly
//! against the library API. That is useful but does NOT exercise:
//!   - The CLI's argument parser and `Command::Serve` branch.
//!   - The actual process lifecycle (spawn, stdin EOF, shutdown hook,
//!     Tantivy commit, SQLite vacuum, process exit).
//!   - The binary's response to licensing / config-dir environment.
//!
//! So we spawn the real binary here, frame MCP messages as line-delimited
//! JSON (rmcp 1.5 stdio codec), and assert the full round-trip.
//!
//! Runtime: ~2-5 s. Skips cleanly on Windows (process signaling + pipe
//! semantics differ; the other shutdown test already exercises the
//! ReconServer lifecycle on every platform).

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Seed a Rust mini-project into `root`. Kept tiny — the binary spawn
/// cost already dominates the test wall time; we don't need 100 files
/// to prove the lifecycle.
fn make_project(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\npub fn mul(a: i32, b: i32) -> i32 { a * b }\n",
    )
    .unwrap();
}

/// Frame an rmcp stdio JSON-RPC message: single-line JSON + trailing LF.
fn frame(id: u64, method: &str, params: serde_json::Value) -> String {
    let msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let mut s = msg.to_string();
    s.push('\n');
    s
}

#[test]
fn binary_spawn_initialize_call_tool_shutdown() {
    // Fresh repo + config dir for full isolation.
    let repo = tempfile::tempdir().expect("repo tmpdir");
    let config = tempfile::tempdir().expect("config tmpdir");
    make_project(repo.path());

    // Seed a signed dev license so `recon serve` doesn't die on
    // `validate_license_or_die()`. The seed uses the built-in dev HMAC
    // key baked in by build.rs when RECON_LICENSE_HMAC_KEY is unset.
    recon_server::license::seed_dev_cache(config.path()).expect("seed_dev_cache");

    // CARGO_BIN_EXE_recon is set by Cargo for integration tests that
    // target a crate with a [[bin]]. This points to the binary that
    // this test run just compiled.
    let bin = env!("CARGO_BIN_EXE_recon");

    let mut child = Command::new(bin)
        .arg("--repo")
        .arg(repo.path())
        .arg("serve")
        // Redirect license/config lookup into our tempdir.
        .env("RECON_CONFIG_DIR", config.path())
        // Quiet the server so stderr doesn't clog the test output.
        .env("RECON_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn recon serve");

    let mut stdin = child.stdin.take().expect("stdin pipe");
    let stdout = child.stdout.take().expect("stdout pipe");
    let mut reader = BufReader::new(stdout);

    let read_response = |reader: &mut BufReader<_>| -> serde_json::Value {
        // Skip log lines that sneak onto stdout (shouldn't happen —
        // tracing goes to stderr — but defensively try a couple of
        // lines and parse the first valid JSON object).
        for _ in 0..3 {
            let mut line = String::new();
            let n = reader.read_line(&mut line).expect("read_line");
            assert!(n > 0, "server closed stdout unexpectedly");
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                return v;
            }
        }
        panic!("no JSON response from server after 3 lines");
    };

    // ── 1. MCP initialize ────────────────────────────────────────────────
    let init = frame(
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "recon-e2e-test", "version": "0.0.0" }
        }),
    );
    stdin.write_all(init.as_bytes()).expect("write initialize");
    stdin.flush().expect("flush initialize");

    let resp = read_response(&mut reader);
    assert_eq!(resp["id"], 1);
    assert!(
        resp["result"]["serverInfo"]["name"]
            .as_str()
            .unwrap_or("")
            .contains("recon"),
        "initialize response should name the server: {resp}"
    );

    // ── 2. initialized notification (no response) ────────────────────────
    let note = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
    })
    .to_string()
        + "\n";
    stdin.write_all(note.as_bytes()).expect("write initialized");
    stdin.flush().expect("flush initialized");

    // ── 3. tools/list — must include the recon code_* tool surface ──────
    let list = frame(2, "tools/list", serde_json::json!({}));
    stdin.write_all(list.as_bytes()).expect("write tools/list");
    stdin.flush().expect("flush tools/list");

    let resp = read_response(&mut reader);
    assert_eq!(resp["id"], 2);
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for expected in ["code_outline", "code_find_symbol", "code_list"] {
        assert!(
            names.contains(&expected),
            "tools/list must expose {expected}; got {names:?}"
        );
    }
    // code_stats and code_savings are operator/CLI tools; v0.4 dropped
    // their MCP `#[tool(...)]` registration so agents don't waste
    // context introspecting their own diagnostics. They remain
    // reachable via `recon stats` / `recon savings show`.
    for hidden in ["code_stats", "code_savings"] {
        assert!(
            !names.contains(&hidden),
            "tools/list must NOT expose {hidden}; got {names:?}"
        );
    }

    // ── 4. tools/call → code_list (cheapest agent-facing tool) ───────────
    let call = frame(
        3,
        "tools/call",
        serde_json::json!({
            "name": "code_list",
            "arguments": {}
        }),
    );
    stdin.write_all(call.as_bytes()).expect("write tools/call");
    stdin.flush().expect("flush tools/call");

    let resp = read_response(&mut reader);
    assert_eq!(resp["id"], 3);
    let content = &resp["result"]["content"];
    assert!(
        content.is_array(),
        "tools/call result.content must be array: {resp}"
    );
    let text = content[0]["text"].as_str().expect("content text");
    let body: serde_json::Value = serde_json::from_str(text).expect("code_list body is JSON");
    // v0.5.0+: code_list wraps its rows in a `Hits(kind="file")` envelope
    // instead of returning a bare array. The hits live under `body.hits`.
    assert_eq!(
        body["shape"].as_str(),
        Some("Hits"),
        "code_list response must be the canonical Hits envelope: {text}"
    );
    assert_eq!(
        body["kind"].as_str(),
        Some("file"),
        "code_list Hits envelope must carry kind=\"file\": {text}"
    );
    let hits = body["hits"].as_array().expect("Hits.hits must be an array");
    assert!(
        !hits.is_empty(),
        "code_list should return at least 1 indexed file on the test project: {text}"
    );

    // ── 5. Close stdin — simulates IDE exit ──────────────────────────────
    // rmcp's RunningService.waiting() resolves on transport close; the
    // CLI's tokio::select! then calls server.shutdown() which drains the
    // watcher, commits Tantivy, and runs incremental_vacuum.
    drop(stdin);

    // ── 6. Wait for clean exit, bounded ──────────────────────────────────
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("recon did not exit within 15 s after stdin close");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    };
    assert!(
        status.success(),
        "recon exited with non-zero status after graceful shutdown: {status:?}"
    );

    // ── 7. DB reopens cleanly — proves shutdown ran vacuum / commit ──────
    let db_path = repo.path().join(".recon").join("index.db");
    assert!(
        db_path.exists(),
        "index.db should exist after a successful serve session"
    );
    let _reopened = recon_storage::store::Store::open(&db_path)
        .expect("index.db must be reopenable after clean shutdown");
}
