//! Real-world benchmark harness — indexes a real repo and measures tool latencies.
//!
//! Usage: `cargo run --release --bin bench-real -- <repo-root>`

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use recon_search::tantivy_backend::TantivyBackend;
use recon_server::server::ReconServer;
use recon_storage::store::Store;

struct BenchResult {
    #[allow(dead_code)]
    name: &'static str,
    #[allow(dead_code)]
    duration_ms: f64,
}

#[allow(dead_code)]
fn bench<F: FnOnce() -> R, R>(name: &'static str, f: F) -> BenchResult {
    let start = Instant::now();
    let _ = f();
    let elapsed = start.elapsed();
    let ms = elapsed.as_secs_f64() * 1000.0;
    let result = BenchResult {
        name,
        duration_ms: ms,
    };
    println!("{:<40} {:>10.1} ms", result.name, result.duration_ms);
    result
}

async fn bench_tool(
    server: &ReconServer,
    name: &'static str,
    tool: &str,
    args: &str,
) -> BenchResult {
    let start = Instant::now();
    let _ = server.query_tool(tool, args).await;
    let elapsed = start.elapsed();
    let ms = elapsed.as_secs_f64() * 1000.0;
    println!("{:<40} {:>10.1} ms", name, ms);
    BenchResult {
        name,
        duration_ms: ms,
    }
}

fn make_server(root: &Path) -> ReconServer {
    let recon_dir = root.join(".recon");
    fs::create_dir_all(&recon_dir).unwrap();

    let db_path = recon_dir.join("recon.db");
    let store = Store::open(&db_path).unwrap();

    let tantivy_dir = recon_dir.join("tantivy");
    fs::create_dir_all(&tantivy_dir).unwrap();
    let tantivy = TantivyBackend::open(&tantivy_dir).unwrap();

    ReconServer::new(root.to_path_buf(), store, tantivy).unwrap()
}

#[tokio::main]
async fn main() {
    let repo = std::env::args()
        .nth(1)
        .expect("Usage: bench-real <repo-root>");
    let repo_path = PathBuf::from(&repo);
    let repo_path = repo_path.canonicalize().expect("repo path must exist");

    println!("═══════════════════════════════════════════════════════");
    println!("  recon benchmark — {}", repo_path.display());
    println!("═══════════════════════════════════════════════════════");
    println!();

    // ── Phase 1: Walk ──────────────────────────────────────────────
    println!("── Phase 1: File walk ──────────────────────────────────");
    let walk_start = Instant::now();
    let paths: Vec<PathBuf> = recon_indexer::walker::walk_repo(&repo_path);
    let walk_ms = walk_start.elapsed().as_secs_f64() * 1000.0;
    let file_count = paths.len();
    println!("{:<40} {:>10.1} ms", "walk_repo", walk_ms);
    println!("  Found {} source files", file_count);
    println!();

    // ── Phase 2: Full indexing ─────────────────────────────────────
    println!("── Phase 2: Full indexing ──────────────────────────────");
    // Purge existing index for clean measurement
    let recon_dir = repo_path.join(".recon");
    if recon_dir.exists() {
        fs::remove_dir_all(&recon_dir).unwrap();
    }

    let server = make_server(&repo_path);

    let index_start = Instant::now();
    server.index_repo().await.unwrap();
    let index_elapsed = index_start.elapsed();
    println!(
        "{:<40} {:>10.1} ms",
        "index_repo (cold)",
        index_elapsed.as_secs_f64() * 1000.0
    );
    println!();

    // ── Phase 3: Stats ─────────────────────────────────────────────
    println!("── Phase 3: Index stats ────────────────────────────────");
    let stats = server.query_tool("code_stats", "{}").await;
    let stats_json: serde_json::Value = serde_json::from_str(&stats).unwrap();
    let files_indexed = stats_json["files_indexed"].as_u64().unwrap_or(0);
    let total_symbols = stats_json["total_symbols"].as_u64().unwrap_or(0);
    let tantivy_docs = stats_json["tantivy_docs"].as_u64().unwrap_or(0);
    println!("  Files indexed:   {}", files_indexed);
    println!("  Total symbols:   {}", total_symbols);
    println!("  Tantivy docs:    {}", tantivy_docs);
    println!();

    // ── Phase 4: Tool latencies (warm cache) ───────────────────────
    println!("── Phase 4: Tool latencies (warm) ──────────────────────");

    bench_tool(
        &server,
        "code_find_symbol (exact)",
        "code_find_symbol",
        r#"{"name": "new", "kind": null, "lang": null}"#,
    )
    .await;

    bench_tool(
        &server,
        "code_find_symbol (fuzzy)",
        "code_find_symbol",
        r#"{"name": "handle_editor", "kind": null, "lang": null}"#,
    )
    .await;

    bench_tool(
        &server,
        "code_search (exact)",
        "code_search",
        r#"{"query": "pub fn", "mode": "exact", "filter": null}"#,
    )
    .await;

    bench_tool(
        &server,
        "code_search (regex)",
        "code_search",
        r#"{"query": "fn\\s+\\w+", "mode": "regex", "filter": null}"#,
    )
    .await;

    bench_tool(
        &server,
        "code_search (hybrid)",
        "code_search",
        r#"{"query": "editor", "mode": "hybrid", "filter": null}"#,
    )
    .await;

    let ls_result = server
        .query_tool(
            "code_list",
            r#"{"lang": null, "filter": null, "glob": null}"#,
        )
        .await;
    let ls_entries: Vec<serde_json::Value> = serde_json::from_str(&ls_result).unwrap();
    let test_file = ls_entries
        .first()
        .and_then(|e| e["path"].as_str())
        .unwrap_or("Cargo.toml");

    bench_tool(
        &server,
        "code_outline",
        "code_outline",
        &serde_json::json!({"path": test_file}).to_string(),
    )
    .await;

    bench_tool(
        &server,
        "code_skeleton",
        "code_skeleton",
        &serde_json::json!({"path": test_file, "depth": 1}).to_string(),
    )
    .await;

    bench_tool(
        &server,
        "code_read_symbol",
        "code_read_symbol",
        &serde_json::json!({"path": test_file, "symbol_or_line": "1"}).to_string(),
    )
    .await;

    bench_tool(
        &server,
        "code_find_refs",
        "code_find_refs",
        r#"{"symbol": "new"}"#,
    )
    .await;

    bench_tool(
        &server,
        "code_repo_map (unfocused)",
        "code_repo_map",
        r#"{"focus_files": null, "token_budget": 2000}"#,
    )
    .await;

    bench_tool(
        &server,
        "code_list (all)",
        "code_list",
        r#"{"lang": null, "filter": null, "glob": null}"#,
    )
    .await;

    bench_tool(
        &server,
        "code_list (rust only)",
        "code_list",
        r#"{"lang": "rust", "filter": null, "glob": null}"#,
    )
    .await;

    bench_tool(
        &server,
        "code_multi_find (3 patterns)",
        "code_multi_find",
        r#"{"patterns": ["fn new", "fn drop", "impl"], "filter": null}"#,
    )
    .await;

    bench_tool(
        &server,
        "code_find_strings",
        "code_find_strings",
        r#"{"pattern": "TODO", "kind": "both", "filter": null}"#,
    )
    .await;

    println!();

    // ── Phase 5: Incremental reindex ───────────────────────────────
    println!("── Phase 5: Incremental reindex (warm) ─────────────────");
    let reindex_start = Instant::now();
    let reindex_result = server
        .query_tool("code_reindex", r#"{"force": false}"#)
        .await;
    let reindex_elapsed = reindex_start.elapsed();
    let reindex_json: serde_json::Value = serde_json::from_str(&reindex_result).unwrap();
    let files_reindexed = reindex_json["files_indexed"].as_u64().unwrap_or(0);
    println!(
        "{:<40} {:>10.1} ms  ({} files re-parsed)",
        "incremental_reindex",
        reindex_elapsed.as_secs_f64() * 1000.0,
        files_reindexed
    );
    println!();

    // ── Phase 6: Index size ────────────────────────────────────────
    println!("── Phase 6: Disk usage ─────────────────────────────────");
    if recon_dir.exists() {
        let db_size = fs::metadata(recon_dir.join("recon.db"))
            .map(|m| m.len())
            .unwrap_or(0);
        let tantivy_size = dir_size(&recon_dir.join("tantivy"));
        let total_size = db_size + tantivy_size;
        println!("  SQLite:          {:.1} MB", db_size as f64 / 1_048_576.0);
        println!(
            "  Tantivy:         {:.1} MB",
            tantivy_size as f64 / 1_048_576.0
        );
        println!(
            "  Total:           {:.1} MB",
            total_size as f64 / 1_048_576.0
        );

        let repo_size = dir_size(&repo_path);
        let ratio = total_size as f64 / repo_size as f64 * 100.0;
        println!(
            "  Repo size:       {:.1} MB",
            repo_size as f64 / 1_048_576.0
        );
        println!("  Index/repo:      {:.1}%", ratio);
    }
    println!();

    println!("═══════════════════════════════════════════════════════");
    println!("  Done.");
    println!("═══════════════════════════════════════════════════════");
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(meta) = entry.metadata() {
                    total += meta.len();
                }
            }
        }
    }
    total
}
