//! Graceful shutdown integration tests.
//!
//! Exercise `ReconServer::shutdown` against a real Store + Tantivy backend —
//! asserts the watcher task stops, the database is reopenable (no corrupt
//! state), and shutdown returns within a bounded wall-clock budget.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use recon_search::tantivy_backend::TantivyBackend;
use recon_server::server::ReconServer;
use recon_storage::store::Store;

fn make_server(root: &Path) -> ReconServer {
    let recon_dir = root.join(".recon");
    fs::create_dir_all(&recon_dir).unwrap();
    let store = Store::open(&recon_dir.join("index.db")).unwrap();
    let tantivy_dir = recon_dir.join("tantivy");
    fs::create_dir_all(&tantivy_dir).unwrap();
    let tantivy = TantivyBackend::open(&tantivy_dir).unwrap();
    ReconServer::new(root.to_path_buf(), store, tantivy).unwrap()
}

#[tokio::test]
async fn shutdown_without_watcher_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
    let server = make_server(dir.path());
    server.index_repo().await.unwrap();

    // Never called start_watcher — shutdown must still flush + vacuum cleanly.
    let start = Instant::now();
    server.shutdown().await;
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "shutdown without watcher took {:?}",
        start.elapsed()
    );

    // Idempotent — calling again is a no-op.
    server.shutdown().await;
}

#[tokio::test]
async fn shutdown_stops_watcher_and_preserves_db() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("lib.rs"), "pub fn answer() -> i32 { 42 }").unwrap();
    let server = make_server(dir.path());
    server.index_repo().await.unwrap();
    server.start_watcher();

    // Give the watcher a moment to start its loop.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let start = Instant::now();
    server.shutdown().await;
    let elapsed = start.elapsed();
    // Watcher polls `shutdown_flag` every ~500 ms, so worst-case ~1 s.
    assert!(
        elapsed < Duration::from_secs(3),
        "shutdown took {elapsed:?} — watcher cancellation is slow"
    );

    // Drop the server to release its read/write handles, then reopen — this
    // proves the DB is not in a half-written / locked state after shutdown.
    drop(server);
    let _reopened = Store::open(&dir.path().join(".recon/index.db")).unwrap();
}
