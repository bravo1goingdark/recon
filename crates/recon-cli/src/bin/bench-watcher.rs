//! Watcher save→query latency harness.
//!
//! Boots a real `ReconServer` against a temp repo, then for each iteration:
//! 1. writes a small Rust file (new content each time so the watcher must reparse),
//! 2. waits a settle window for the watcher batch to land,
//! 3. fires a representative tool call (`code_outline`) and times it.
//!
//! Reports p50 / p95 / p99 across the iterations. Also runs a one-shot 50-file
//! burst and reports the watcher batch wall time end-to-end (sample-once).
//!
//! Usage:
//!   cargo run --release -p recon-cli --bin bench-watcher -- [iterations]
//!
//! Default iterations: 100. Increase for tighter percentiles.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use recon_search::tantivy_backend::TantivyBackend;
use recon_server::server::ReconServer;
use recon_storage::store::Store;

fn temp_repo_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "recon-bench-watcher-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_server(root: &Path) -> ReconServer {
    let recon_dir = root.join(".recon");
    fs::create_dir_all(&recon_dir).unwrap();
    let store = Store::open(&recon_dir.join("index.db")).unwrap();
    let tantivy_dir = recon_dir.join("tantivy");
    fs::create_dir_all(&tantivy_dir).unwrap();
    let tantivy = TantivyBackend::open(&tantivy_dir).unwrap();
    ReconServer::new(root.to_path_buf(), store, tantivy).unwrap()
}

fn percentile(sorted_ms: &[f64], p: f64) -> f64 {
    if sorted_ms.is_empty() {
        return 0.0;
    }
    let idx = ((sorted_ms.len() as f64 - 1.0) * p).round() as usize;
    sorted_ms[idx.min(sorted_ms.len() - 1)]
}

#[tokio::main]
async fn main() {
    let iterations: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);

    println!("═══════════════════════════════════════════════════════");
    println!("  watcher save→query benchmark — {iterations} iterations");
    println!("═══════════════════════════════════════════════════════");

    let dir = temp_repo_dir();
    let root = dir.as_path();

    // Seed the repo with enough source files that the cache snapshot is
    // non-trivial — mirrors a small real project.
    for i in 0..200 {
        let content =
            format!("pub fn seed_fn_{i}() -> u32 {{ {i} }}\npub struct Seed{i} {{ x: u32 }}\n");
        fs::write(root.join(format!("seed_{i}.rs")), content).unwrap();
    }

    let server = make_server(root);
    server.index_repo().await.unwrap();
    server.start_watcher();

    // Brief warm-up so the cache fills + the watcher loop is in steady state.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Pick one of the seed files as the test target.
    let target = "seed_0.rs";

    println!("\n── Single-file save → code_outline latency ─────────────");
    let mut latencies_ms: Vec<f64> = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let new_content =
            format!("pub fn iter_{i}() -> u32 {{ {i} }}\npub struct Iter{i} {{ x: u32 }}\n");
        fs::write(root.join(target), new_content).unwrap();

        // Settle: 250ms watcher debounce + a small margin for the write phase.
        tokio::time::sleep(Duration::from_millis(900)).await;

        let args = serde_json::json!({ "path": target }).to_string();
        let start = Instant::now();
        let _ = server.query_tool("code_outline", &args).await;
        latencies_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    latencies_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "  code_outline    p50 {:>8.2} ms   p95 {:>8.2} ms   p99 {:>8.2} ms   max {:>8.2} ms",
        percentile(&latencies_ms, 0.50),
        percentile(&latencies_ms, 0.95),
        percentile(&latencies_ms, 0.99),
        latencies_ms.last().copied().unwrap_or(0.0),
    );

    // ── 50-file burst → watcher batch wall time ─────────────────────
    println!("\n── 50-file burst → indexed-confirm wall time ───────────");
    // Touch 50 files in rapid succession.
    let burst_start = Instant::now();
    for i in 0..50 {
        let content = format!(
            "pub fn burst_{i}() -> u32 {{ {} }}\npub struct Burst{i} {{ y: u32 }}\n",
            i + 1000
        );
        fs::write(root.join(format!("seed_{i}.rs")), content).unwrap();
    }
    let burst_write_ms = burst_start.elapsed().as_secs_f64() * 1000.0;
    println!("  filesystem writes:           {burst_write_ms:>8.2} ms");

    // Poll until all 50 are reflected — query a sentinel symbol from the
    // last-written file. If never observed within 30s, fail loud.
    let sentinel_args = serde_json::json!({
        "name": "burst_49",
        "kind": null,
        "lang": null
    })
    .to_string();
    let observe_start = Instant::now();
    let mut observed = false;
    while observe_start.elapsed() < Duration::from_secs(30) {
        let response = server.query_tool("code_find_symbol", &sentinel_args).await;
        if response.contains("burst_49") {
            observed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let observe_ms = observe_start.elapsed().as_secs_f64() * 1000.0;
    if observed {
        println!("  watcher → queryable (50f):   {observe_ms:>8.2} ms");
    } else {
        println!("  watcher → queryable (50f):   TIMEOUT after {observe_ms:.0} ms");
    }

    server.shutdown().await;
    let _ = fs::remove_dir_all(&dir);
    println!("\n═══════════════════════════════════════════════════════");
}
