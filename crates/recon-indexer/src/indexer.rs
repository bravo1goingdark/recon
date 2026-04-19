//! Core indexing logic: parallel parse with pooled parsers, batch store + Tantivy.

use crate::walker;
use rayon::prelude::*;
use recon_core::error::Error;
use recon_core::lang::Language;
use recon_core::symbol::{FileMeta, Ref, Symbol};
use recon_parser::extract;
use recon_parser::pool::LanguagePools;
use recon_search::tantivy_backend::TantivyBackend;
use recon_storage::hash;
use recon_storage::store::Store;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// Result of parsing a single file (before storing).
pub struct ParsedFile {
    /// File metadata (path, hash, timestamps).
    pub meta: FileMeta,
    /// Extracted symbol definitions.
    pub symbols: Vec<Symbol>,
    /// Extracted symbol references.
    pub refs: Vec<Ref>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Parse a single file using pooled parsers. Does NOT touch the store.
pub fn parse_file_with_content(
    content: &[u8],
    path: &Path,
    repo_root: &Path,
    pools: &LanguagePools,
) -> Option<ParsedFile> {
    let rel_path = path.strip_prefix(repo_root).unwrap_or(path);
    let lang = Language::from_path(path);
    if lang == Language::Unknown {
        return None;
    }

    let content_hash = hash::blake3_bytes(content);
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
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

    let extracted = match pools.get(lang) {
        Some(pool) => extract::extract_symbols_pooled(content, lang, rel_path, pool),
        None => extract::extract_symbols(content, lang, rel_path),
    };

    Some(ParsedFile {
        meta,
        symbols: extracted.symbols,
        refs: extracted.refs,
    })
}

/// Index a single file: read once, hash, parse, store in SQLite + Tantivy.
pub fn index_file(
    store: &Store,
    tantivy: Option<&TantivyBackend>,
    tantivy_writer: Option<&mut tantivy::IndexWriter>,
    path: &Path,
    repo_root: &Path,
) -> Result<(), Error> {
    let rel_path = path.strip_prefix(repo_root).unwrap_or(path);
    let lang = Language::from_path(path);
    if lang == Language::Unknown || walker::is_generated(path) {
        return Ok(());
    }

    let content = std::fs::read(path)?;
    let content_hash = hash::blake3_bytes(&content);

    if let Some(existing_hash) = store.get_file_hash(rel_path)? {
        if existing_hash == content_hash {
            return Ok(());
        }
    }

    let pools = LanguagePools::new(1);
    if let Some(parsed) = parse_file_with_content(&content, path, repo_root, &pools) {
        store.batch_index_file(&parsed.meta, &parsed.symbols, &parsed.refs)?;

        // Also index into Tantivy
        if let (Some(tb), Some(writer)) = (tantivy, tantivy_writer) {
            let _ = tb.index_symbols(writer, rel_path, &parsed.symbols);
        }

        debug!(
            ?rel_path,
            symbols = parsed.symbols.len(),
            refs = parsed.refs.len(),
            "indexed"
        );
    }
    Ok(())
}

/// Index all files in a repo — parallel parse, sequential batch store + Tantivy.
pub fn index_repo(
    store: &Store,
    tantivy: Option<&TantivyBackend>,
    repo_root: &Path,
) -> Result<IndexStats, Error> {
    let paths = walker::walk_repo(repo_root);
    info!(files = paths.len(), "starting repo indexing");

    let pools = Arc::new(LanguagePools::new(rayon::current_num_threads().max(4)));

    // Phase 1: Parallel read + parse
    let parsed: Vec<_> = paths
        .par_iter()
        .filter_map(|path| {
            if walker::is_generated(path) {
                return None;
            }
            let content = match std::fs::read(path) {
                Ok(c) => c,
                Err(e) => {
                    warn!(?path, "read error: {e}");
                    return None;
                }
            };
            parse_file_with_content(&content, path, repo_root, &pools)
        })
        .collect();

    // Phase 2: Sequential batch store (SQLite single-writer + Tantivy single-writer)
    let mut tantivy_writer = tantivy.and_then(|tb| tb.writer(50_000_000).ok());
    let mut stats = IndexStats::default();

    for parsed_file in &parsed {
        match store.batch_index_file(&parsed_file.meta, &parsed_file.symbols, &parsed_file.refs) {
            Ok(()) => {
                stats.files_indexed += 1;
                // Index into Tantivy
                if let (Some(tb), Some(ref mut writer)) = (tantivy, tantivy_writer.as_mut()) {
                    let _ = tb.index_symbols(writer, &parsed_file.meta.path, &parsed_file.symbols);
                }
            }
            Err(e) => {
                warn!(path = ?parsed_file.meta.path, "store error: {e}");
                stats.errors += 1;
            }
        }
    }

    // Commit Tantivy
    if let (Some(tb), Some(ref mut writer)) = (tantivy, tantivy_writer.as_mut()) {
        if let Err(e) = tb.commit(writer) {
            warn!("tantivy commit error: {e}");
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

/// Index a repo with Merkle-based incremental optimization.
///
/// 1. If HEAD matches the last indexed commit → skip entirely.
/// 2. Otherwise, build a new Merkle snapshot and diff against the previous one.
/// 3. Only reindex changed files; cascade-delete removed files.
/// 4. If no previous snapshot exists, falls back to full `index_repo`.
/// 5. Saves the new Merkle snapshot and HEAD SHA after indexing.
pub fn index_repo_incremental(
    store: &Store,
    tantivy: Option<&TantivyBackend>,
    repo_root: &Path,
) -> Result<IndexStats, Error> {
    let store_dir = repo_root.join(".recon");
    let snap_path = store_dir.join("merkle_snapshot.json");

    let last_commit = store.get_meta("last_indexed_commit")?;
    let current_head = match crate::git::head_sha(repo_root) {
        Ok(sha) => Some(sha),
        Err(e) => {
            debug!("gix head_sha unavailable, will do full index: {e}");
            None
        }
    };

    // Fast path: HEAD unchanged since last index
    if let (Some(ref last), Some(ref current)) = (&last_commit, &current_head) {
        if last == current {
            let total = store.symbol_count().unwrap_or(0);
            info!(head = %current, symbols = total, "HEAD matches last index, skipping");
            return Ok(IndexStats {
                files_indexed: 0,
                total_symbols: total,
                errors: 0,
            });
        }
    }

    // Walk and hash all current files
    let paths = walker::walk_repo(repo_root);
    let file_hashes: Vec<(std::path::PathBuf, [u8; 32])> = paths
        .iter()
        .filter_map(|p| {
            let content = std::fs::read(p).ok()?;
            let rel = p.strip_prefix(repo_root).unwrap_or(p).to_path_buf();
            Some((rel, hash::blake3_bytes(&content)))
        })
        .collect();

    let new_snapshot = crate::merkle::MerkleSnapshot::build(file_hashes);

    // Try incremental diff against previous snapshot
    let prev_snapshot = crate::merkle::MerkleSnapshot::load(&snap_path).ok();
    let stats = if let Some(ref prev) = prev_snapshot {
        let diff = new_snapshot.diff(prev);
        if diff.changed.is_empty() && diff.deleted.is_empty() {
            let total = store.symbol_count().unwrap_or(0);
            info!(symbols = total, "merkle diff: no changes");
            IndexStats {
                files_indexed: 0,
                total_symbols: total,
                errors: 0,
            }
        } else {
            info!(
                changed = diff.changed.len(),
                deleted = diff.deleted.len(),
                "merkle diff: incremental reindex"
            );
            index_diff(store, tantivy, repo_root, &diff)?
        }
    } else {
        info!("no previous merkle snapshot, full index");
        index_repo(store, tantivy, repo_root)?
    };

    // Save new snapshot and HEAD
    let _ = std::fs::create_dir_all(&store_dir);
    if let Err(e) = new_snapshot.save(&snap_path) {
        warn!("failed to save merkle snapshot: {e}");
    }
    if let Some(sha) = &current_head {
        if let Err(e) = store.set_meta("last_indexed_commit", sha) {
            warn!("failed to store last_indexed_commit: {e}");
        }
    }

    Ok(stats)
}

/// Index only the files identified by a Merkle diff.
fn index_diff(
    store: &Store,
    tantivy: Option<&TantivyBackend>,
    repo_root: &Path,
    diff: &crate::merkle::MerkleDiff,
) -> Result<IndexStats, Error> {
    let pools = Arc::new(LanguagePools::new(rayon::current_num_threads().max(4)));
    let mut tantivy_writer = tantivy.and_then(|tb| tb.writer(50_000_000).ok());
    let mut stats = IndexStats::default();

    // Delete removed files
    for rel_path in &diff.deleted {
        if let Err(e) = store.delete_file_cascade(rel_path) {
            warn!(?rel_path, "delete cascade error: {e}");
            stats.errors += 1;
        }
    }

    // Parse and index changed files (parallel parse, sequential store)
    let abs_changed: Vec<std::path::PathBuf> =
        diff.changed.iter().map(|p| repo_root.join(p)).collect();
    let parsed: Vec<_> = abs_changed
        .par_iter()
        .filter_map(|path| {
            if walker::is_generated(path) {
                return None;
            }
            let content = match std::fs::read(path) {
                Ok(c) => c,
                Err(e) => {
                    warn!(?path, "read error: {e}");
                    return None;
                }
            };
            parse_file_with_content(&content, path, repo_root, &pools)
        })
        .collect();

    for parsed_file in &parsed {
        match store.batch_index_file(&parsed_file.meta, &parsed_file.symbols, &parsed_file.refs) {
            Ok(()) => {
                stats.files_indexed += 1;
                if let (Some(tb), Some(ref mut writer)) = (tantivy, tantivy_writer.as_mut()) {
                    let _ = tb.index_symbols(writer, &parsed_file.meta.path, &parsed_file.symbols);
                }
            }
            Err(e) => {
                warn!(path = ?parsed_file.meta.path, "store error: {e}");
                stats.errors += 1;
            }
        }
    }

    if let (Some(tb), Some(ref mut writer)) = (tantivy, tantivy_writer.as_mut()) {
        if let Err(e) = tb.commit(writer) {
            warn!("tantivy commit error: {e}");
        }
    }

    stats.total_symbols = store.symbol_count().unwrap_or(0);
    info!(
        files = stats.files_indexed,
        deleted = diff.deleted.len(),
        symbols = stats.total_symbols,
        "incremental indexing complete"
    );
    Ok(stats)
}

/// Stats from an indexing run.
#[derive(Debug, Default)]
pub struct IndexStats {
    /// Number of files that were parsed and stored.
    pub files_indexed: usize,
    /// Total symbols in the store after indexing.
    pub total_symbols: u64,
    /// Number of files that errored during indexing.
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
        index_file(&store, None, None, &src, dir.path()).unwrap();

        let count = store.symbol_count().unwrap();
        assert!(count >= 2, "expected at least 2 symbols, got {count}");
    }

    #[test]
    fn index_skips_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("lib.rs");
        std::fs::write(&src, "pub fn hello() {}").unwrap();

        let store = Store::open_memory().unwrap();
        index_file(&store, None, None, &src, dir.path()).unwrap();
        let count1 = store.symbol_count().unwrap();

        index_file(&store, None, None, &src, dir.path()).unwrap();
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
        let stats = index_repo(&store, None, dir.path()).unwrap();

        assert_eq!(stats.files_indexed, 2);
        assert!(stats.total_symbols >= 2);
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn index_repo_incremental_stores_commit() {
        let dir = tempfile::tempdir().unwrap();
        // Initialize a git repo with a file
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let store = Store::open_memory().unwrap();
        let stats = index_repo_incremental(&store, None, dir.path()).unwrap();
        assert!(stats.files_indexed >= 1);

        let commit = store.get_meta("last_indexed_commit").unwrap();
        assert!(commit.is_some(), "should store last_indexed_commit");

        // Second run with same HEAD should skip
        let stats2 = index_repo_incremental(&store, None, dir.path()).unwrap();
        assert_eq!(stats2.files_indexed, 0, "should skip when HEAD unchanged");
    }

    #[test]
    fn index_repo_with_tantivy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.rs"),
            "pub fn hello() {}\npub struct Config {}",
        )
        .unwrap();

        let store = Store::open_memory().unwrap();
        let tantivy = TantivyBackend::open_memory().unwrap();
        let stats = index_repo(&store, Some(&tantivy), dir.path()).unwrap();

        assert!(stats.files_indexed >= 1);
        // Tantivy should have docs
        assert!(
            tantivy.doc_count() >= 2,
            "tantivy should have indexed symbols"
        );

        // Search should work
        let hits = tantivy.search("hello", 10).unwrap();
        assert!(!hits.is_empty(), "should find hello in tantivy");
    }
}
