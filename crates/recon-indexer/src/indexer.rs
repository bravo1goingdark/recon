//! Core indexing logic: parallel parse, batch store.

use crate::walker;
use recon_core::error::Error;
use recon_core::lang::Language;
use recon_core::symbol::{FileMeta, Ref, Symbol};
use recon_parser::extract;
use recon_storage::hash;
use recon_storage::store::Store;
use rayon::prelude::*;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// Result of parsing a single file (before storing).
struct ParsedFile {
    meta: FileMeta,
    symbols: Vec<Symbol>,
    refs: Vec<Ref>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Parse a single file: hash, extract symbols. Does NOT touch the store.
fn parse_file(path: &Path, repo_root: &Path) -> Result<Option<ParsedFile>, Error> {
    let rel_path = path.strip_prefix(repo_root).unwrap_or(path);

    let lang = Language::from_path(path);
    if lang == Language::Unknown {
        return Ok(None);
    }

    if walker::is_generated(path) {
        debug!(?path, "skipping generated file");
        return Ok(None);
    }

    let content = std::fs::read(path)?;
    let content_hash = hash::blake3_bytes(&content);

    let fs_meta = std::fs::metadata(path)?;
    let mtime = fs_meta
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let meta = FileMeta {
        path: rel_path.to_path_buf(),
        lang,
        size_bytes: content.len() as u64,
        content_hash,
        mtime,
        indexed_at: now_secs(),
    };

    let extracted = extract::extract_symbols(&content, lang, rel_path);

    Ok(Some(ParsedFile {
        meta,
        symbols: extracted.symbols,
        refs: extracted.refs,
    }))
}

/// Index a single file: hash, parse, store symbols.
pub fn index_file(store: &Store, path: &Path, repo_root: &Path) -> Result<(), Error> {
    let rel_path = path.strip_prefix(repo_root).unwrap_or(path);

    // Skip if unchanged
    let content = std::fs::read(path)?;
    let content_hash = hash::blake3_bytes(&content);
    if let Some(existing_hash) = store.get_file_hash(rel_path)? {
        if existing_hash == content_hash {
            debug!(?rel_path, "unchanged, skipping");
            return Ok(());
        }
    }

    if let Some(parsed) = parse_file(path, repo_root)? {
        store.batch_index_file(&parsed.meta, &parsed.symbols, &parsed.refs)?;
        debug!(
            ?rel_path,
            symbols = parsed.symbols.len(),
            refs = parsed.refs.len(),
            "indexed"
        );
    }
    Ok(())
}

/// Index all files in a repo — parallel parse, sequential store.
pub fn index_repo(store: &Store, repo_root: &Path) -> Result<IndexStats, Error> {
    let paths = walker::walk_repo(repo_root);
    info!(files = paths.len(), "starting repo indexing");

    // Phase 1: Parallel parse (CPU-bound, no DB access)
    let parsed: Vec<_> = paths
        .par_iter()
        .filter_map(|path| {
            match parse_file(path, repo_root) {
                Ok(Some(p)) => Some(Ok(p)),
                Ok(None) => None,
                Err(e) => {
                    warn!(?path, "parse error: {e}");
                    Some(Err(e))
                }
            }
        })
        .collect();

    // Phase 2: Sequential batch store (single-writer SQLite)
    let mut stats = IndexStats::default();
    for result in parsed {
        match result {
            Ok(parsed_file) => {
                match store.batch_index_file(
                    &parsed_file.meta,
                    &parsed_file.symbols,
                    &parsed_file.refs,
                ) {
                    Ok(()) => stats.files_indexed += 1,
                    Err(e) => {
                        warn!(path = ?parsed_file.meta.path, "store error: {e}");
                        stats.errors += 1;
                    }
                }
            }
            Err(_) => stats.errors += 1,
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

        assert_eq!(stats.files_indexed, 2);
        assert!(stats.total_symbols >= 2);
        assert_eq!(stats.errors, 0);
    }
}
