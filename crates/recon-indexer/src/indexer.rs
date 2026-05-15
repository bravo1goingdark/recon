//! Core indexing logic: parallel parse with pooled parsers, batch store + Tantivy.

use crate::merkle::{MerkleDiff, MerkleSnapshot};
use crate::walker;
use rayon::prelude::*;
use recon_core::error::Error;
use recon_core::lang::Language;
use recon_core::redact;
use recon_core::symbol::{FileMeta, Ref, Symbol};
use recon_parser::extract;
use recon_parser::pool::LanguagePools;
use recon_search::tantivy_backend::TantivyBackend;
use recon_storage::hash;
use recon_storage::store::Store;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, instrument, warn};

/// Result of parsing a single file (before storing).
pub struct ParsedFile {
    /// File metadata (path, hash, timestamps).
    pub meta: FileMeta,
    /// Extracted symbol definitions.
    pub symbols: Vec<Symbol>,
    /// Extracted symbol references.
    pub refs: Vec<Ref>,
}

/// Path to the persisted Merkle snapshot within the repo.
const MERKLE_SNAPSHOT_PATH: &str = ".recon/merkle.json";

/// Tunable indexing options loaded from `.recon/config.toml` by callers.
#[derive(Debug, Clone, Copy)]
pub struct IndexOptions {
    /// Maximum source file size to index.
    pub max_file_size: u64,
    /// Tantivy writer heap size in bytes.
    pub tantivy_heap_bytes: usize,
    /// Whether sensitive paths such as `.env` and private keys may be indexed.
    pub allow_sensitive: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            max_file_size: 2_097_152,
            tantivy_heap_bytes: 50_000_000,
            allow_sensitive: false,
        }
    }
}

fn is_sensitive_path(path: &Path, repo_root: &Path) -> bool {
    let rel = path.strip_prefix(repo_root).unwrap_or(path);
    redact::is_blocked_path_in_repo(rel, repo_root)
}

/// Resolve the merkle snapshot path for a given repo root.
fn merkle_snapshot_path(repo_root: &Path) -> PathBuf {
    repo_root.join(MERKLE_SNAPSHOT_PATH)
}

/// Build a MerkleSnapshot by hashing all indexable files in parallel.
///
/// Walks the repo using [`walker::walk_repo`], reads each file, filters out
/// generated content, and computes blake3 content hashes. The resulting
/// snapshot maps relative paths to (content hash, mtime).
pub fn build_merkle_snapshot(repo_root: &Path) -> MerkleSnapshot {
    build_merkle_snapshot_with_options(repo_root, IndexOptions::default())
}

/// Build a MerkleSnapshot with configurable walker limits.
pub fn build_merkle_snapshot_with_options(
    repo_root: &Path,
    options: IndexOptions,
) -> MerkleSnapshot {
    let paths: Vec<_> = walker::walk_repo_with_limit(repo_root, options.max_file_size)
        .into_iter()
        .filter(|p| options.allow_sensitive || !is_sensitive_path(p, repo_root))
        .collect();
    let entries: Vec<_> = paths
        .par_iter()
        .filter_map(|path| {
            let content = match std::fs::read(path) {
                Ok(c) => c,
                Err(e) => {
                    warn!(?path, "read error during merkle build: {e}");
                    return None;
                }
            };
            if walker::is_generated_content(&content) {
                return None;
            }
            let rel = match path.strip_prefix(repo_root) {
                Ok(r) => r.to_path_buf(),
                Err(_) => path.clone(),
            };
            let content_hash = hash::blake3_bytes(&content);
            let mtime = mtime_of(path);
            Some((rel, content_hash, mtime))
        })
        .collect();
    MerkleSnapshot::build(entries)
}

/// Load the previous Merkle snapshot from the repo, if it exists.
fn load_previous_snapshot(repo_root: &Path) -> Option<MerkleSnapshot> {
    let snap_path = merkle_snapshot_path(repo_root);
    match MerkleSnapshot::load(&snap_path) {
        Ok(snap) => {
            debug!(entries = snap.len(), "loaded previous merkle snapshot");
            Some(snap)
        }
        Err(_) => None,
    }
}

/// Save the current Merkle snapshot to the repo.
fn save_snapshot(repo_root: &Path, snapshot: &MerkleSnapshot) {
    let snap_path = merkle_snapshot_path(repo_root);
    if let Err(e) = std::fs::create_dir_all(snap_path.parent().unwrap_or(repo_root)) {
        warn!("failed to create .recon directory: {e}");
        return;
    }
    if let Err(e) = snapshot.save(&snap_path) {
        warn!("failed to save merkle snapshot: {e}");
    } else {
        debug!(entries = snapshot.len(), "saved merkle snapshot");
    }
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
/// Uses millisecond precision to detect rapid modifications.
pub fn mtime_of(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
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

/// Per-file result from Phase 1 parallel processing.
struct FileResult {
    parsed: Option<ParsedFile>,
    rel_path: PathBuf,
    /// Tracks what happened to this file for Merkle snapshot construction.
    snapshot_state: SnapshotState,
    mtime: i64,
}

/// Tracks what we know about a file's content hash after Phase 1.
enum SnapshotState {
    /// Hash computed this run — use it directly.
    Known([u8; 32]),
    /// mtime matched previous snapshot — reuse previous entry.
    MtimeSkipped,
    /// Generated content, read error, or unknown language — omit from snapshot.
    Excluded,
}

/// Index all files in a repo — parallel parse, sequential batch store + Tantivy.
/// Full repo index. If `shared_writer` is provided, uses it instead of creating
/// a new IndexWriter (avoids LockBusy when a watcher already holds the lock).
///
/// On subsequent runs, loads the previous Merkle snapshot and skips files whose
/// content hash has not changed, making cold-start re-indexing faster.
///
/// Builds the new Merkle snapshot from Phase 1 data — no second full repo read.
#[allow(clippy::needless_option_as_deref)]
pub fn index_repo(
    store: &Store,
    tantivy: Option<&TantivyBackend>,
    repo_root: &Path,
    shared_writer: Option<&mut tantivy::IndexWriter>,
) -> Result<IndexStats, Error> {
    index_repo_with_options(
        store,
        tantivy,
        repo_root,
        shared_writer,
        IndexOptions::default(),
    )
}

/// Index all files in a repo using caller-provided config options.
#[allow(clippy::needless_option_as_deref)]
pub fn index_repo_with_options(
    store: &Store,
    tantivy: Option<&TantivyBackend>,
    repo_root: &Path,
    shared_writer: Option<&mut tantivy::IndexWriter>,
    options: IndexOptions,
) -> Result<IndexStats, Error> {
    let paths: Vec<_> = walker::walk_repo_with_limit(repo_root, options.max_file_size)
        .into_iter()
        .filter(|p| options.allow_sensitive || !is_sensitive_path(p, repo_root))
        .collect();
    info!(files = paths.len(), "starting repo indexing");

    // Enter high-throughput indexing mode for faster bulk inserts
    store.enter_indexing_mode()?;

    let pools = Arc::new(LanguagePools::new(rayon::current_num_threads().max(4)));

    // Load previous Merkle snapshot for change detection
    let previous_snapshot = load_previous_snapshot(repo_root);

    // Phase 1: Parallel read + parse. Returns a FileResult for every path so we
    // can build the new Merkle snapshot without a second full repo walk.
    let all_results: Vec<FileResult> = paths
        .par_iter()
        .map(|path| {
            let rel = path.strip_prefix(repo_root).unwrap_or(path).to_path_buf();
            let mtime = mtime_of(path);

            // Mtime pre-filter: skip if mtime matches snapshot (no read needed)
            if let Some(ref prev) = previous_snapshot {
                if prev.is_unchanged(&rel, mtime) {
                    return FileResult {
                        parsed: None,
                        rel_path: rel,
                        snapshot_state: SnapshotState::MtimeSkipped,
                        mtime,
                    };
                }
            }

            let content = match std::fs::read(path) {
                Ok(c) => c,
                Err(e) => {
                    warn!(?path, "read error: {e}");
                    return FileResult {
                        parsed: None,
                        rel_path: rel,
                        snapshot_state: SnapshotState::Excluded,
                        mtime,
                    };
                }
            };
            if walker::is_generated_content(&content) {
                return FileResult {
                    parsed: None,
                    rel_path: rel,
                    snapshot_state: SnapshotState::Excluded,
                    mtime,
                };
            }
            let content_hash = hash::blake3_bytes(&content);

            // Double-check: skip if hash matches previous snapshot
            if let Some(ref prev) = previous_snapshot {
                if let Some(prev_hash) = prev.get_hash(&rel) {
                    if prev_hash == content_hash {
                        return FileResult {
                            parsed: None,
                            rel_path: rel,
                            snapshot_state: SnapshotState::Known(content_hash),
                            mtime,
                        };
                    }
                }
            }

            let parsed =
                parse_file_with_content(&content, path, repo_root, &pools, content_hash, mtime);
            FileResult {
                parsed,
                rel_path: rel,
                snapshot_state: SnapshotState::Known(content_hash),
                mtime,
            }
        })
        .collect();

    // Phase 2: Bulk store — chunked transactions (500 files each) for safety + speed.
    let mut stats = IndexStats::default();
    const CHUNK_SIZE: usize = 500;

    let to_store: Vec<&ParsedFile> = all_results
        .iter()
        .filter_map(|r| r.parsed.as_ref())
        .collect();

    for chunk in to_store.chunks(CHUNK_SIZE) {
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
        tantivy.and_then(|tb| match tb.writer(options.tantivy_heap_bytes) {
            Ok(w) => Some(w),
            Err(e) => {
                warn!(
                    %e,
                    "tantivy writer creation failed during full reindex; \
                     BM25 docs will not be updated this run"
                );
                None
            }
        })
    } else {
        None
    };
    let writer_ref = shared_writer.or(local_writer.as_mut());

    if let (Some(tb), Some(writer)) = (tantivy, writer_ref) {
        // Single commit at the end. Tantivy's writer heap (50 MB above)
        // already flushes internal segments when full; an explicit
        // interim commit only adds visible segments to the index, and
        // every extra segment costs query latency at search time. One
        // commit at the end produces ~1–2 segments for a 300K-symbol
        // cold index instead of ~15 with interim commits every 20K docs.
        for r in &all_results {
            if let Some(ref pf) = r.parsed {
                let _ = tb.index_symbols(writer, &pf.meta.path, &pf.symbols);
            }
        }
        if let Err(e) = tb.commit(writer) {
            warn!("tantivy commit error: {e}");
        }
    }

    // Build and save the new Merkle snapshot from Phase 1 data — no second full read.
    let new_snapshot = {
        let mut entries = Vec::with_capacity(all_results.len());
        for r in &all_results {
            match r.snapshot_state {
                SnapshotState::Known(hash) => {
                    entries.push((r.rel_path.clone(), hash, r.mtime));
                }
                SnapshotState::MtimeSkipped => {
                    // Use previous snapshot's hash (mtime unchanged → content unchanged)
                    if let Some(ref prev) = previous_snapshot {
                        if let Some(prev_hash) = prev.get_hash(&r.rel_path) {
                            entries.push((r.rel_path.clone(), prev_hash, r.mtime));
                        }
                    }
                }
                SnapshotState::Excluded => {
                    // Generated content, read errors, or unknown language — omit
                }
            }
        }
        MerkleSnapshot::build(entries)
    };
    save_snapshot(repo_root, &new_snapshot);

    // Restore safe SQLite defaults and flush WAL
    store.exit_indexing_mode()?;

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
/// 3. Non-git repos or first index → fall back to Merkle diff.
/// 4. If gix operations fail → fall back to Merkle diff instead of full index.
/// 5. Only changed files are read, parsed, and stored.
///
/// If `shared_writer` is provided, uses it for Tantivy writes instead of
/// creating a new writer (prevents LockBusy).
#[allow(clippy::needless_option_as_deref)]
#[instrument(skip(store, tantivy, shared_writer), fields(repo = %repo_root.display()))]
pub fn index_repo_incremental(
    store: &Store,
    tantivy: Option<&TantivyBackend>,
    repo_root: &Path,
    mut shared_writer: Option<&mut tantivy::IndexWriter>,
) -> Result<IndexStats, Error> {
    index_repo_incremental_with_options(
        store,
        tantivy,
        repo_root,
        shared_writer.as_deref_mut(),
        IndexOptions::default(),
    )
}

/// Incremental index using caller-provided config options.
#[allow(clippy::needless_option_as_deref)]
#[instrument(skip(store, tantivy, shared_writer), fields(repo = %repo_root.display()))]
pub fn index_repo_incremental_with_options(
    store: &Store,
    tantivy: Option<&TantivyBackend>,
    repo_root: &Path,
    mut shared_writer: Option<&mut tantivy::IndexWriter>,
    options: IndexOptions,
) -> Result<IndexStats, Error> {
    use std::collections::HashSet;

    let last_commit = store.get_meta("last_indexed_commit")?;

    // Open the repo once — all git operations share this handle
    let repo = match crate::git::open_repo(repo_root) {
        Ok(r) => Some(r),
        Err(e) => {
            debug!("gix open unavailable, will try merkle diff: {e}");
            None
        }
    };

    let current_head = match repo.as_ref().map(crate::git::head_sha_with_repo) {
        Some(Ok(sha)) => Some(sha),
        Some(Err(e)) => {
            debug!("gix head_sha unavailable, will try merkle diff: {e}");
            None
        }
        None => None,
    };

    // Non-git directory or first index: fall back to Merkle diff
    let current_head = match current_head {
        Some(sha) => sha,
        None => {
            info!("not a git repo or no HEAD, using merkle diff");
            return index_repo_merkle_fallback(
                store,
                tantivy,
                repo_root,
                shared_writer.as_deref_mut(),
                options,
            );
        }
    };
    // `repo` is Some because we got a valid current_head from it above.
    // Fall back to Merkle diff rather than panic if this invariant is somehow violated.
    let Some(repo) = repo else {
        warn!("git repo handle unexpectedly missing after HEAD was resolved; using merkle diff");
        return index_repo_merkle_fallback(
            store,
            tantivy,
            repo_root,
            shared_writer.as_deref_mut(),
            options,
        );
    };

    if last_commit.is_none() {
        info!("no previous index, using merkle diff");
        let stats = index_repo_merkle_fallback(
            store,
            tantivy,
            repo_root,
            shared_writer.as_deref_mut(),
            options,
        )?;
        if let Err(e) = store.set_meta("last_indexed_commit", &current_head) {
            warn!("failed to store last_indexed_commit: {e}");
        }
        return Ok(stats);
    }
    // `last_commit` is Some because the is_none() branch returned above.
    let Some(last_commit) = last_commit else {
        warn!("last_commit unexpectedly None after non-None check; using merkle diff");
        return index_repo_merkle_fallback(
            store,
            tantivy,
            repo_root,
            shared_writer.as_deref_mut(),
            options,
        );
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
                warn!("gix tree diff failed, falling back to merkle diff: {e}");
                return index_repo_merkle_fallback(
                    store,
                    tantivy,
                    repo_root,
                    shared_writer.as_deref_mut(),
                    options,
                );
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
                && (options.allow_sensitive || !is_sensitive_path(p, repo_root))
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
        options,
    )?;

    if let Err(e) = store.set_meta("last_indexed_commit", &current_head) {
        warn!("failed to store last_indexed_commit: {e}");
    }

    Ok(stats)
}

/// Incremental indexing fallback using Merkle snapshot diff.
///
/// Used when git operations are unavailable or fail, and for non-git repos.
/// Compares the current file tree against the saved Merkle snapshot and
/// only re-indexes files whose content hash has changed.
#[allow(clippy::needless_option_as_deref)]
fn index_repo_merkle_fallback(
    store: &Store,
    tantivy: Option<&TantivyBackend>,
    repo_root: &Path,
    shared_writer: Option<&mut tantivy::IndexWriter>,
    options: IndexOptions,
) -> Result<IndexStats, Error> {
    let current_snapshot = build_merkle_snapshot_with_options(repo_root, options);
    let previous_snapshot = load_previous_snapshot(repo_root);

    let diff = match &previous_snapshot {
        Some(prev) => current_snapshot.diff(prev),
        None => {
            // No previous snapshot — index everything
            MerkleDiff {
                changed: current_snapshot.entries.keys().cloned().collect(),
                deleted: Vec::new(),
            }
        }
    };

    if diff.changed.is_empty() && diff.deleted.is_empty() {
        let total = store.symbol_count().unwrap_or(0);
        info!(symbols = total, "merkle diff: no changes detected");
        save_snapshot(repo_root, &current_snapshot);
        return Ok(IndexStats {
            files_indexed: 0,
            total_symbols: total,
            errors: 0,
        });
    }

    info!(
        changed = diff.changed.len(),
        deleted = diff.deleted.len(),
        "merkle diff: incremental reindex"
    );

    // Convert relative paths to absolute for indexing
    let changed_abs: Vec<PathBuf> = diff.changed.iter().map(|rel| repo_root.join(rel)).collect();
    let deleted_abs: Vec<PathBuf> = diff.deleted.iter().map(|rel| repo_root.join(rel)).collect();

    let stats = index_diff(
        store,
        tantivy,
        repo_root,
        &changed_abs,
        &deleted_abs,
        shared_writer,
        options,
    )?;

    // Save the new snapshot after successful indexing
    save_snapshot(repo_root, &current_snapshot);

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
    options: IndexOptions,
) -> Result<IndexStats, Error> {
    let pools = Arc::new(LanguagePools::new(rayon::current_num_threads().max(4)));
    // Use shared writer if available, else create a local one
    let mut local_writer = if shared_writer.is_none() {
        tantivy.and_then(|tb| match tb.writer(options.tantivy_heap_bytes) {
            Ok(w) => Some(w),
            Err(e) => {
                warn!(
                    %e,
                    "tantivy writer creation failed during incremental index; \
                     BM25 docs for changed files will not be updated this run"
                );
                None
            }
        })
    } else {
        None
    };
    let mut tantivy_writer: Option<&mut tantivy::IndexWriter> =
        shared_writer.or(local_writer.as_mut());
    let mut stats = IndexStats::default();

    // Delete removed files in one transaction (BEGIN/COMMIT once instead of N).
    if !deleted.is_empty() {
        let rel_paths: Vec<&Path> = deleted
            .iter()
            .map(|p| p.strip_prefix(repo_root).unwrap_or(p))
            .collect();
        if let Err(e) = store.delete_files_cascade(&rel_paths) {
            warn!(count = rel_paths.len(), "delete cascade error: {e}");
            stats.errors += rel_paths.len();
        }
    }

    // Parse and index changed files (parallel parse, sequential store)
    let parsed: Vec<_> = changed
        .par_iter()
        .filter_map(|path| {
            if !options.allow_sensitive && is_sensitive_path(path, repo_root) {
                return None;
            }
            if std::fs::metadata(path).is_ok_and(|m| m.len() > options.max_file_size) {
                return None;
            }
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

        let store = Store::open(&dir.path().join("index-a.db")).unwrap();
        let stats = index_repo(&store, None, dir.path(), None).unwrap();

        assert_eq!(stats.files_indexed, 2);
        assert!(stats.total_symbols >= 2);
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn index_options_honor_file_size_and_sensitive_toggles() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("large.rs"), "fn oversized() {}\n").unwrap();
        std::fs::write(dir.path().join("id_rsa.rs"), "fn secret_source() {}").unwrap();

        let store = Store::open(&dir.path().join("index-b.db")).unwrap();
        let stats = index_repo_with_options(
            &store,
            None,
            dir.path(),
            None,
            IndexOptions {
                max_file_size: 13,
                allow_sensitive: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(stats.files_indexed, 1);
        assert!(store.get_file_hash(Path::new("main.rs")).unwrap().is_some());
        assert!(store
            .get_file_hash(Path::new("large.rs"))
            .unwrap()
            .is_none());
        assert!(store
            .get_file_hash(Path::new("id_rsa.rs"))
            .unwrap()
            .is_none());

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("large.rs"), "fn oversized() {}\n").unwrap();
        std::fs::write(dir.path().join("id_rsa.rs"), "fn secret_source() {}").unwrap();
        let store = Store::open(&dir.path().join("index-b.db")).unwrap();
        let stats = index_repo_with_options(
            &store,
            None,
            dir.path(),
            None,
            IndexOptions {
                max_file_size: 64,
                allow_sensitive: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(stats.files_indexed, 3);
        assert!(store
            .get_file_hash(Path::new("id_rsa.rs"))
            .unwrap()
            .is_some());
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
        // A plain directory (not a git repo) must fall back to merkle diff without panicking.
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
    fn merkle_fallback_on_non_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn foo() {}").unwrap();
        std::fs::write(dir.path().join("util.rs"), "pub fn util() {}").unwrap();

        let store = Store::open_memory().unwrap();

        // First run: should index all files and save snapshot
        let stats = index_repo_incremental(&store, None, dir.path(), None).unwrap();
        assert!(stats.files_indexed >= 2);

        // Verify snapshot was saved
        let snap_path = dir.path().join(".recon/merkle.json");
        assert!(snap_path.exists(), "merkle snapshot should be saved");

        // Second run: no changes, should skip
        let stats2 = index_repo_incremental(&store, None, dir.path(), None).unwrap();
        assert_eq!(stats2.files_indexed, 0, "should skip when no changes");

        // Modify one file
        std::fs::write(dir.path().join("lib.rs"), "pub fn foo() { println!(); }").unwrap();

        // Third run: should only re-index the changed file
        let stats3 = index_repo_incremental(&store, None, dir.path(), None).unwrap();
        assert_eq!(
            stats3.files_indexed, 1,
            "should only re-index 1 changed file"
        );
    }

    #[test]
    fn merkle_detects_deleted_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("extra.rs"), "pub fn extra() {}").unwrap();

        let store = Store::open_memory().unwrap();
        let stats = index_repo_incremental(&store, None, dir.path(), None).unwrap();
        assert!(stats.files_indexed >= 2);
        let count_before = store.symbol_count().unwrap();

        // Delete a file
        std::fs::remove_file(dir.path().join("extra.rs")).unwrap();

        let _stats2 = index_repo_incremental(&store, None, dir.path(), None).unwrap();
        let count_after = store.symbol_count().unwrap();
        assert!(
            count_after < count_before,
            "symbols should decrease after deletion: before={count_before}, after={count_after}"
        );
    }

    #[test]
    fn build_merkle_snapshot_works() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn b() {}").unwrap();

        let snapshot = build_merkle_snapshot(dir.path());
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.entries.contains_key(&PathBuf::from("a.rs")));
        assert!(snapshot.entries.contains_key(&PathBuf::from("b.rs")));
    }

    #[test]
    fn index_repo_skips_unchanged_with_merkle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn lib() {}").unwrap();

        let store = Store::open_memory().unwrap();

        // First run
        let stats1 = index_repo(&store, None, dir.path(), None).unwrap();
        assert!(stats1.files_indexed >= 2);

        // Second run with no changes — should skip all files
        let stats2 = index_repo(&store, None, dir.path(), None).unwrap();
        assert_eq!(stats2.files_indexed, 0, "should skip all unchanged files");

        // Modify one file
        std::fs::write(dir.path().join("main.rs"), "fn main() { println!(); }").unwrap();

        // Third run — should only index the changed file
        let stats3 = index_repo(&store, None, dir.path(), None).unwrap();
        assert_eq!(stats3.files_indexed, 1, "should only index 1 changed file");
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
