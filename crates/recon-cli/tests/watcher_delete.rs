//! Integration tests for the file watcher's delete-event handling.
//!
//! Regression coverage for the bug where `Watcher`'s filter dropped events for
//! deleted paths (`p.is_file()` returned false), leaving stale symbols in the
//! SQLite, Tantivy, and vector stores until a force reindex.

use std::fs;
use std::path::Path;
use std::time::Duration;

use recon_search::tantivy_backend::TantivyBackend;
use recon_server::server::ReconServer;
use recon_storage::read_pool::ReadPool;
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

/// Open a separate ReadPool on the same db — WAL allows concurrent readers.
fn read_pool_for(root: &Path) -> ReadPool {
    ReadPool::new(&root.join(".recon/index.db"), 2).unwrap()
}

/// Wait long enough for at least one debounce flush + write phase.
async fn settle() {
    // 250 ms debounce + up to 500 ms recv_timeout + processing margin.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
}

#[tokio::test]
async fn watcher_removes_symbols_on_file_delete() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("doomed.rs");
    fs::write(&file_path, "pub fn watcher_delete_doomed_zz1() {}").unwrap();

    let server = make_server(dir.path());
    server.index_repo().await.unwrap();
    server.start_watcher();

    let pool = read_pool_for(dir.path());

    // Pre-condition: symbol is present after the cold index.
    let before = pool.symbols_for_path(Path::new("doomed.rs")).unwrap();
    assert!(
        before.iter().any(|s| s.name == "watcher_delete_doomed_zz1"),
        "expected symbol present before delete, got {:?}",
        before.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
    );

    // Delete the file — the watcher must observe and cascade.
    fs::remove_file(&file_path).unwrap();
    settle().await;

    let after = pool.symbols_for_path(Path::new("doomed.rs")).unwrap();
    assert!(
        after.is_empty(),
        "symbols should be gone after delete, got {:?}",
        after.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
    );

    // File row should also be gone (delete_file_cascade drops the files row).
    let hash = pool.get_file_hash(Path::new("doomed.rs")).unwrap();
    assert!(hash.is_none(), "file_hash should be cleared after delete");

    server.shutdown().await;
}

#[tokio::test]
async fn watcher_handles_rename_as_delete_plus_create() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("renamed_from.rs");
    let new_path = dir.path().join("renamed_to.rs");
    fs::write(&old_path, "pub fn watcher_rename_zzz_marker() {}").unwrap();

    let server = make_server(dir.path());
    server.index_repo().await.unwrap();
    server.start_watcher();

    let pool = read_pool_for(dir.path());

    let before = pool.symbols_for_path(Path::new("renamed_from.rs")).unwrap();
    assert!(
        before.iter().any(|s| s.name == "watcher_rename_zzz_marker"),
        "expected symbol present before rename"
    );

    fs::rename(&old_path, &new_path).unwrap();
    settle().await;

    let old = pool.symbols_for_path(Path::new("renamed_from.rs")).unwrap();
    assert!(
        old.is_empty(),
        "old path's symbols should be gone after rename, got {:?}",
        old.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
    );

    let new = pool.symbols_for_path(Path::new("renamed_to.rs")).unwrap();
    assert!(
        new.iter().any(|s| s.name == "watcher_rename_zzz_marker"),
        "new path's symbols should be present after rename, got {:?}",
        new.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
    );

    server.shutdown().await;
}
