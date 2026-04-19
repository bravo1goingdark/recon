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

    // Start the server — use workspace root as repo
    let mut child = Command::new(&binary)
        .args(["serve", "--repo", workspace_root.to_str().unwrap()])
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

    // Use a thread to read with timeout
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

    // Collect lines for up to 3 seconds
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => lines_read.push(line),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if !lines_read.is_empty() {
                    break; // Got at least one response, stop waiting
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
        assert!(
            parsed.is_ok(),
            "stdout line {i} is not valid JSON: {line}"
        );

        let value = parsed.unwrap();
        // Should be a JSON-RPC response (has "jsonrpc" field)
        assert!(
            value.get("jsonrpc").is_some(),
            "stdout line {i} is not JSON-RPC: {line}"
        );
    }
}
