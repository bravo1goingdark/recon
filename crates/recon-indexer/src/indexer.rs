//! Core indexing logic: parse files, extract symbols, store in SQLite.

use crate::walker;
use recon_core::error::Error;
use recon_core::lang::Language;
use recon_core::symbol::FileMeta;
use recon_parser::extract;
use recon_storage::hash;
use recon_storage::store::Store;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// Index a single file: hash, parse, store symbols.
pub fn index_file(store: &Store, path: &Path, repo_root: &Path) -> Result<(), Error> {
    let rel_path = path
        .strip_prefix(repo_root)
        .unwrap_or(path);

    let lang = Language::from_path(path);
    if lang == Language::Unknown {
        return Ok(());
    }

    // Check if generated
    if walker::is_generated(path) {
        debug!(?path, "skipping generated file");
        return Ok(());
    }

    // Read and hash
    let content = std::fs::read(path).map_err(|e| {
        warn!(?path, "failed to read file: {e}");
        e
    })?;
    let content_hash = hash::blake3_bytes(&content);

    // Skip if unchanged
    if let Some(existing_hash) = store.get_file_hash(rel_path)? {
        if existing_hash == content_hash {
            debug!(?rel_path, "unchanged, skipping");
            return Ok(());
        }
    }

    let meta = std::fs::metadata(path)?;
    let mtime = meta
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Upsert file metadata
    store.upsert_file(&FileMeta {
        path: rel_path.to_path_buf(),
        lang,
        size_bytes: content.len() as u64,
        content_hash,
        mtime,
        indexed_at: now,
    })?;

    // Delete old symbols for this file
    store.delete_symbols_for_path(rel_path)?;

    // Extract and store new symbols
    let extracted = extract::extract_symbols(&content, lang, rel_path);

    for sym in &extracted.symbols {
        store.upsert_symbol(sym)?;
    }

    if !extracted.refs.is_empty() {
        store.upsert_refs(&extracted.refs)?;
    }

    debug!(
        ?rel_path,
        symbols = extracted.symbols.len(),
        refs = extracted.refs.len(),
        "indexed"
    );
    Ok(())
}

/// Index all files in a repo.
pub fn index_repo(store: &Store, repo_root: &Path) -> Result<IndexStats, Error> {
    let paths = walker::walk_repo(repo_root);
    info!(files = paths.len(), "starting repo indexing");

    let mut stats = IndexStats::default();
    for path in &paths {
        match index_file(store, path, repo_root) {
            Ok(()) => stats.files_indexed += 1,
            Err(e) => {
                warn!(?path, "index error: {e}");
                stats.errors += 1;
            }
        }
    }

    stats.total_symbols = store.symbol_count().unwrap_or(0);
    info!(
        files = stats.files_indexed,
        symbols = stats.total_symbols,
        errors = stats.errors,
        "indexing complete"
    );
    Ok(stats)
}

/// Stats from an indexing run.
#[derive(Debug, Default)]
pub struct IndexStats {
    pub files_indexed: usize,
    pub total_symbols: u64,
    pub errors: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("lib.rs");
        std::fs::write(&src, "pub fn hello() {}\npub struct Foo { pub x: i32 }").unwrap();

        let store = Store::open_memory().unwrap();
        index_file(&store, &src, dir.path()).unwrap();

        let count = store.symbol_count().unwrap();
        assert!(count >= 2, "expected at least 2 symbols, got {count}");
    }

    #[test]
    fn index_skips_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("lib.rs");
        std::fs::write(&src, "pub fn hello() {}").unwrap();

        let store = Store::open_memory().unwrap();
        index_file(&store, &src, dir.path()).unwrap();
        let count1 = store.symbol_count().unwrap();

        // Re-index same file — should skip
        index_file(&store, &src, dir.path()).unwrap();
        let count2 = store.symbol_count().unwrap();
        assert_eq!(count1, count2);
    }

    #[test]
    fn index_repo_works() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("lib.py"), "def foo(): pass").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "just a note").unwrap();

        let store = Store::open_memory().unwrap();
        let stats = index_repo(&store, dir.path()).unwrap();

        assert_eq!(stats.files_indexed, 2); // .rs and .py, not .txt
        assert!(stats.total_symbols >= 2);
        assert_eq!(stats.errors, 0);
    }
}
