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
use std::path::{Path, PathBuf};
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
///
/// Accepts a pre-computed `content_hash` and `mtime` to avoid redundant
/// blake3 rehashing and `metadata()` syscalls when the caller already has them.
pub fn parse_file_with_content(
    content: &[u8],
    path: &Path,
    repo_root: &Path,
    pools: &LanguagePools,
    content_hash: [u8; 32],
    mtime: i64,
) -> Option<ParsedFile> {
    let rel_path = path.strip_prefix(repo_root).unwrap_or(path);
    let lang = Language::from_path(path);
    if lang == Language::Unknown {
        return None;
    }

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

/// Read mtime from a path, returning 0 on failure.
pub fn mtime_of(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Index a single file: read once, hash, parse, store in SQLite + Tantivy.
///
/// Returns `Ok(true)` if the file was actually indexed, `Ok(false)` if skipped
/// (unknown language, generated file, unchanged content hash, or parse failure).
pub fn index_file(
    store: &Store,
    tantivy: Option<&TantivyBackend>,
    tantivy_writer: Option<&mut tantivy::IndexWriter>,
    path: &Path,
    repo_root: &Path,
    pools: Option<&LanguagePools>,
) -> Result<bool, Error> {
    let rel_path = path.strip_prefix(repo_root).unwrap_or(path);
    let lang = Language::from_path(path);
    if lang == Language::Unknown {
        return Ok(false);
    }

    let content = std::fs::read(path)?;
    if walker::is_generated_content(&content) {
        return Ok(false);
    }
    let content_hash = hash::blake3_bytes(&content);

    if let Some(existing_hash) = store.get_file_hash(rel_path)? {
        if existing_hash == content_hash {
            return Ok(false);
        }
    }

    let owned_pools;
    let pools = match pools {
        Some(p) => p,
        None => {
            owned_pools = LanguagePools::new(1);
            &owned_pools
        }
    };
    let mtime = mtime_of(path);
    if let Some(parsed) =
        parse_file_with_content(&content, path, repo_root, pools, content_hash, mtime)
    {
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
        return Ok(true);
    }
    Ok(false)
}

/// Index all files in a repo — parallel parse, sequential batch store + Tantivy.
/// Full repo index. If `shared_writer` is provided, uses it instead of creating
/// a new IndexWriter (avoids LockBusy when a watcher already holds the lock).
pub fn index_repo(
    store: &Store,
    tantivy: Option<&TantivyBackend>,
    repo_root: &Path,
    shared_writer: Option<&mut tantivy::IndexWriter>,
) -> Result<IndexStats, Error> {
    let paths = walker::walk_repo(repo_root);
    info!(files = paths.len(), "starting repo indexing");

    let pools = Arc::new(LanguagePools::new(rayon::current_num_threads().max(4)));

    // Phase 1: Parallel read + parse
    let parsed: Vec<_> = paths
        .par_iter()
        .filter_map(|path| {
            let content = match std::fs::read(path) {
                Ok(c) => c,
                Err(e) => {
                    warn!(?path, "read error: {e}");
                    return None;
                }
            };
            if walker::is_generated_content(&content) {
                return None;
            }
            let content_hash = hash::blake3_bytes(&content);
            let mtime = mtime_of(path);
            parse_file_with_content(&content, path, repo_root, &pools, content_hash, mtime)
        })
        .collect();

    // Phase 2: Bulk store — chunked transactions (500 files each) for safety + speed.
    let mut stats = IndexStats::default();
    const CHUNK_SIZE: usize = 500;

    for chunk in parsed.chunks(CHUNK_SIZE) {
        let bulk: Vec<_> = chunk
            .iter()
            .map(|p| (&p.meta, p.symbols.as_slice(), p.refs.as_slice()))
            .collect();

        match store.batch_index_files(&bulk) {
            Ok(()) => {
                stats.files_indexed += chunk.len();
            }
            Err(e) => {
                warn!(chunk_size = chunk.len(), "bulk store error: {e}");
                stats.errors += chunk.len();
            }
        }
    }

    // Tantivy indexing — use shared writer if available, else create a local one
    let mut local_writer = if shared_writer.is_none() {
        tantivy.and_then(|tb| tb.writer(50_000_000).ok())
    } else {
        None
    };
    let writer_ref = shared_writer.or(local_writer.as_mut());

    if let (Some(tb), Some(writer)) = (tantivy, writer_ref) {
        let mut docs_since_commit = 0usize;
        for parsed_file in &parsed {
            let _ = tb.index_symbols(writer, &parsed_file.meta.path, &parsed_file.symbols);
            docs_since_commit += parsed_file.symbols.len();
            if docs_since_commit >= 20_000 {
                if let Err(e) = tb.commit(writer) {
                    warn!("tantivy interim commit error: {e}");
                }
                docs_since_commit = 0;
            }
        }
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

/// Index a repo incrementally using gix tree diff + worktree status.
///
/// 1. If HEAD matches the last indexed commit → only check worktree status.
/// 2. If HEAD differs → gix tree diff (old..new) + worktree status.
/// 3. Non-git repos or first index → fall back to full `index_repo`.
/// 4. Only changed files are read, parsed, and stored.
///
/// If `shared_writer` is provided, uses it for Tantivy writes instead of
/// creating a new writer (prevents LockBusy).
#[allow(clippy::needless_option_as_deref)]
pub fn index_repo_incremental(
    store: &Store,
    tantivy: Option<&TantivyBackend>,
    repo_root: &Path,
    mut shared_writer: Option<&mut tantivy::IndexWriter>,
) -> Result<IndexStats, Error> {
    use std::collections::HashSet;

    let last_commit = store.get_meta("last_indexed_commit")?;

    // Open the repo once — all git operations share this handle
    let repo = match crate::git::open_repo(repo_root) {
        Ok(r) => Some(r),
        Err(e) => {
            debug!("gix open unavailable, will do full index: {e}");
            None
        }
    };

    let current_head = match repo.as_ref().map(crate::git::head_sha_with_repo) {
        Some(Ok(sha)) => Some(sha),
        Some(Err(e)) => {
            debug!("gix head_sha unavailable, will do full index: {e}");
            None
        }
        None => None,
    };

    // Non-git directory or first index: fall back to full scan
    let current_head = match current_head {
        Some(sha) => sha,
        None => {
            info!("not a git repo, full index");
            return index_repo(store, tantivy, repo_root, shared_writer.as_deref_mut());
        }
    };
    // `repo` is Some because we got a valid current_head from it above.
    // Fall back to full index rather than panic if this invariant is somehow violated.
    let Some(repo) = repo else {
        warn!("git repo handle unexpectedly missing after HEAD was resolved; doing full index");
        return index_repo(store, tantivy, repo_root, shared_writer.as_deref_mut());
    };

    if last_commit.is_none() {
        info!("no previous index, full index");
        let stats = index_repo(store, tantivy, repo_root, shared_writer.as_deref_mut())?;
        if let Err(e) = store.set_meta("last_indexed_commit", &current_head) {
            warn!("failed to store last_indexed_commit: {e}");
        }
        return Ok(stats);
    }
    // `last_commit` is Some because the is_none() branch returned above.
    let Some(last_commit) = last_commit else {
        warn!("last_commit unexpectedly None after non-None check; doing full index");
        return index_repo(store, tantivy, repo_root, shared_writer.as_deref_mut());
    };

    // Get committed changes (tree diff) if HEAD advanced
    let mut all_modified: HashSet<PathBuf> = HashSet::new();
    let mut all_deleted: HashSet<PathBuf> = HashSet::new();

    if last_commit != current_head {
        match crate::git::diff_commits_with_repo(&repo, repo_root, &last_commit, &current_head) {
            Ok(diff) => {
                for p in diff.modified {
                    all_modified.insert(p);
                }
                for p in diff.deleted {
                    all_deleted.insert(p);
                }
            }
            Err(e) => {
                warn!("gix tree diff failed, falling back to full index: {e}");
                let stats = index_repo(store, tantivy, repo_root, shared_writer.as_deref_mut())?;
                if let Err(e) = store.set_meta("last_indexed_commit", &current_head) {
                    warn!("failed to store last_indexed_commit: {e}");
                }
                return Ok(stats);
            }
        }
    }

    // Also pick up uncommitted worktree changes
    match crate::git::status_changed_paths_with_repo(&repo, repo_root) {
        Ok(status) => {
            for p in status.modified {
                all_modified.insert(p);
            }
            for p in status.deleted {
                all_deleted.insert(p);
            }
        }
        Err(e) => {
            debug!("gix status failed (non-fatal): {e}");
        }
    }

    // A path modified then deleted = deleted only
    all_modified.retain(|p| !all_deleted.contains(p));

    // Filter to indexable source files before checking emptiness
    let modified: Vec<PathBuf> = all_modified
        .into_iter()
        .filter(|p| {
            Language::from_path(p) != Language::Unknown
                && !walker::is_vendored(&p.to_string_lossy())
        })
        .collect();
    let deleted: Vec<PathBuf> = all_deleted
        .into_iter()
        .filter(|p| Language::from_path(p) != Language::Unknown)
        .collect();

    if modified.is_empty() && deleted.is_empty() {
        let total = store.symbol_count().unwrap_or(0);
        info!(head = %current_head, symbols = total, "HEAD matches last index, skipping");
        if let Err(e) = store.set_meta("last_indexed_commit", &current_head) {
            warn!("failed to store last_indexed_commit: {e}");
        }
        return Ok(IndexStats {
            files_indexed: 0,
            total_symbols: total,
            errors: 0,
        });
    }

    info!(
        changed = modified.len(),
        deleted = deleted.len(),
        "gix diff: incremental reindex"
    );

    let stats = index_diff(
        store,
        tantivy,
        repo_root,
        &modified,
        &deleted,
        shared_writer.as_deref_mut(),
    )?;

    if let Err(e) = store.set_meta("last_indexed_commit", &current_head) {
        warn!("failed to store last_indexed_commit: {e}");
    }

    Ok(stats)
}

/// Index only specific changed and deleted files.
#[allow(clippy::needless_option_as_deref)]
fn index_diff(
    store: &Store,
    tantivy: Option<&TantivyBackend>,
    repo_root: &Path,
    changed: &[PathBuf],
    deleted: &[PathBuf],
    shared_writer: Option<&mut tantivy::IndexWriter>,
) -> Result<IndexStats, Error> {
    let pools = Arc::new(LanguagePools::new(rayon::current_num_threads().max(4)));
    // Use shared writer if available, else create a local one
    let mut local_writer = if shared_writer.is_none() {
        tantivy.and_then(|tb| tb.writer(15_000_000).ok())
    } else {
        None
    };
    let mut tantivy_writer: Option<&mut tantivy::IndexWriter> =
        shared_writer.or(local_writer.as_mut());
    let mut stats = IndexStats::default();

    // Delete removed files — convert to relative paths for store
    for abs_path in deleted {
        let rel_path = abs_path.strip_prefix(repo_root).unwrap_or(abs_path);
        if let Err(e) = store.delete_file_cascade(rel_path) {
            warn!(?rel_path, "delete cascade error: {e}");
            stats.errors += 1;
        }
    }

    // Parse and index changed files (parallel parse, sequential store)
    let parsed: Vec<_> = changed
        .par_iter()
        .filter_map(|path| {
            let content = match std::fs::read(path) {
                Ok(c) => c,
                Err(e) => {
                    warn!(?path, "read error: {e}");
                    return None;
                }
            };
            if walker::is_generated_content(&content) {
                return None;
            }
            let content_hash = hash::blake3_bytes(&content);
            let mtime = mtime_of(path);
            parse_file_with_content(&content, path, repo_root, &pools, content_hash, mtime)
        })
        .collect();

    for parsed_file in &parsed {
        match store.batch_index_file(&parsed_file.meta, &parsed_file.symbols, &parsed_file.refs) {
            Ok(()) => {
                stats.files_indexed += 1;
                if let (Some(tb), Some(writer)) = (tantivy, tantivy_writer.as_deref_mut()) {
                    let _ = tb.index_symbols(writer, &parsed_file.meta.path, &parsed_file.symbols);
                }
            }
            Err(e) => {
                warn!(path = ?parsed_file.meta.path, "store error: {e}");
                stats.errors += 1;
            }
        }
    }

    if let (Some(tb), Some(writer)) = (tantivy, tantivy_writer.as_deref_mut()) {
        if let Err(e) = tb.commit(writer) {
            warn!("tantivy commit error: {e}");
        }
    }

    stats.total_symbols = store.symbol_count().unwrap_or(0);
    info!(
        files = stats.files_indexed,
        deleted = deleted.len(),
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
        let indexed = index_file(&store, None, None, &src, dir.path(), None).unwrap();
        assert!(indexed, "expected file to be indexed");

        let count = store.symbol_count().unwrap();
        assert!(count >= 2, "expected at least 2 symbols, got {count}");
    }

    #[test]
    fn index_skips_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("lib.rs");
        std::fs::write(&src, "pub fn hello() {}").unwrap();

        let store = Store::open_memory().unwrap();
        let indexed = index_file(&store, None, None, &src, dir.path(), None).unwrap();
        assert!(indexed, "first index should succeed");
        let count1 = store.symbol_count().unwrap();

        let indexed = index_file(&store, None, None, &src, dir.path(), None).unwrap();
        assert!(!indexed, "second index should skip (unchanged hash)");
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
        let stats = index_repo(&store, None, dir.path(), None).unwrap();

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
        let stats = index_repo_incremental(&store, None, dir.path(), None).unwrap();
        assert!(stats.files_indexed >= 1);

        let commit = store.get_meta("last_indexed_commit").unwrap();
        assert!(commit.is_some(), "should store last_indexed_commit");

        // Second run with same HEAD should skip
        let stats2 = index_repo_incremental(&store, None, dir.path(), None).unwrap();
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
        let stats = index_repo(&store, Some(&tantivy), dir.path(), None).unwrap();

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

    /// Helper: run a git command in a directory.
    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Helper: init a temp git repo with initial files and first commit.
    fn init_test_repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init"]);
        git(dir.path(), &["config", "user.email", "test@test.com"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        for (name, content) in files {
            std::fs::write(dir.path().join(name), content).unwrap();
        }
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "init"]);
        dir
    }

    #[test]
    fn incremental_uses_git_diff() {
        let dir = init_test_repo(&[
            ("main.rs", "fn main() {}"),
            ("lib.rs", "pub fn lib() {}"),
            ("util.rs", "pub fn util() {}"),
        ]);

        // First index: full
        let store = Store::open_memory().unwrap();
        let stats = index_repo_incremental(&store, None, dir.path(), None).unwrap();
        assert!(stats.files_indexed >= 3, "first run should index all files");

        // Modify only one file, commit
        std::fs::write(dir.path().join("main.rs"), "fn main() { println!(); }").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "update main"]);

        let stats2 = index_repo_incremental(&store, None, dir.path(), None).unwrap();
        assert_eq!(
            stats2.files_indexed, 1,
            "second run should only index the 1 changed file, got {}",
            stats2.files_indexed
        );
    }

    #[test]
    fn incremental_picks_up_worktree_changes() {
        let dir = init_test_repo(&[("main.rs", "fn main() {}")]);

        let store = Store::open_memory().unwrap();
        let stats = index_repo_incremental(&store, None, dir.path(), None).unwrap();
        assert!(stats.files_indexed >= 1);

        // Modify file WITHOUT committing — worktree change only
        std::fs::write(dir.path().join("main.rs"), "fn main() { todo!(); }").unwrap();

        // HEAD is same, but worktree has changes → should still index
        let stats2 = index_repo_incremental(&store, None, dir.path(), None).unwrap();
        assert_eq!(
            stats2.files_indexed, 1,
            "uncommitted worktree change should be detected"
        );
    }

    #[test]
    fn incremental_on_non_git_dir_falls_back_to_full_index() {
        // A plain directory (not a git repo) must fall back to full index without panicking.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn foo() {}").unwrap();

        let store = Store::open_memory().unwrap();
        let stats = index_repo_incremental(&store, None, dir.path(), None).unwrap();
        assert!(
            stats.files_indexed >= 1,
            "full fallback should index the file"
        );
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn incremental_handles_deleted_file() {
        let dir = init_test_repo(&[
            ("main.rs", "fn main() {}"),
            ("extra.rs", "pub fn extra() {}"),
        ]);

        let store = Store::open_memory().unwrap();
        let stats = index_repo_incremental(&store, None, dir.path(), None).unwrap();
        assert!(stats.files_indexed >= 2);
        let count_before = store.symbol_count().unwrap();
        assert!(count_before >= 2);

        // Delete extra.rs and commit
        std::fs::remove_file(dir.path().join("extra.rs")).unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "delete extra"]);

        let stats2 = index_repo_incremental(&store, None, dir.path(), None).unwrap();
        let count_after = store.symbol_count().unwrap();
        assert!(
            count_after < count_before,
            "symbols should decrease after deletion: before={count_before}, after={count_after}"
        );
        assert_eq!(stats2.errors, 0);
    }
}
