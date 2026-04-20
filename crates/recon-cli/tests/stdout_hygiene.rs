//! Stdout hygiene test: verify `recon serve` never writes non-JSON-RPC to stdout.
//!
//! This is critical because stdio transport uses stdout as the JSON-RPC channel.
//! Any stray println!, library banner, or default logger output corrupts the protocol.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn serve_stdout_is_clean_jsonrpc() {
    // Build the binary first
    let status = Command::new("cargo")
        .args(["build", "--bin", "recon"])
        .status()
        .expect("cargo build failed");
    assert!(status.success(), "cargo build failed");

    // Binary is in workspace root's target dir
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let binary = workspace_root.join("target/debug/recon");

    if !binary.exists() {
        panic!("recon binary not found at {}", binary.display());
    }

    // Create a temp dir with a pre-seeded license cache so the CLI starts
    // without network access (key required but cache provides offline grace)
    let tmp = tempfile::tempdir().unwrap();
    let recon_dir = tmp.path().join(".recon");
    std::fs::create_dir_all(&recon_dir).unwrap();
    let cache = serde_json::json!({
        "cached_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        "response": {
            "valid": true,
            "tier": "Free",
            "limits": { "max_repos": 1, "max_files": 250, "max_loc": 5000 },
            "expires_at": 0,
            "message": "test"
        }
    });
    std::fs::write(
        recon_dir.join("license.json"),
        serde_json::to_string(&cache).unwrap(),
    )
    .unwrap();

    // Start the server with a dummy key + unreachable API (cache provides fallback)
    let mut child = Command::new(&binary)
        .args([
            "serve",
            "--repo",
            tmp.path().to_str().unwrap(),
            "--key",
            "sk-recon-test-00000000000000000000",
        ])
        .env("RECON_API_URL", "http://127.0.0.1:1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn recon serve");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    // Send an MCP initialize request
    let init_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "test-client",
                "version": "1.0.0"
            }
        }
    });
    let msg = serde_json::to_string(&init_request).unwrap();
    writeln!(stdin, "{msg}").unwrap();
    stdin.flush().unwrap();

    // Read lines from stdout with a timeout
    let reader = BufReader::new(stdout);
    let mut lines_read = Vec::new();

    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        for line in reader.lines() {
            match line {
                Ok(l) if !l.is_empty() => {
                    let _ = tx.send(l);
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
    });

    // Collect lines for up to 5 seconds
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => lines_read.push(line),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if !lines_read.is_empty() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Kill the server
    let _ = child.kill();
    let _ = child.wait();
    drop(handle);

    // Verify every line on stdout is valid JSON
    assert!(
        !lines_read.is_empty(),
        "server produced no stdout output at all"
    );

    for (i, line) in lines_read.iter().enumerate() {
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(line);
        assert!(parsed.is_ok(), "stdout line {i} is not valid JSON: {line}");

        let value = parsed.unwrap();
        assert!(
            value.get("jsonrpc").is_some(),
            "stdout line {i} is not JSON-RPC: {line}"
        );
    }
}
