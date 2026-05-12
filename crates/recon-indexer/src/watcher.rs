//! File watcher using notify with debouncing.
//!
//! On notify overflow errors, falls back to `gix status` to discover
//! changed paths instead of losing events silently.
//!
//! High-volume directories (`target/`, `node_modules/`, `.git/`, `.recon/`,
//! `.idea/`, `.vscode/`) are excluded from the watch tree itself rather than
//! filtered post-event — this prevents `cargo build` storms from saturating
//! the inotify event queue (Linux default 16,384) and silently dropping the
//! source-file edits we care about.

use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use recon_core::lang::Language;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use tracing::{debug, warn};

/// Directory names that should never be watched recursively.
///
/// `target/` is the dominant contributor to inotify queue overflows during
/// Rust builds. `.git/` churns during pulls/rebases. `node_modules/` and the
/// IDE caches are similarly noisy. `.recon/` is our own index — events there
/// would feed back into the watcher.
fn is_ignored_dir(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some("target")
            | Some("node_modules")
            | Some(".git")
            | Some(".recon")
            | Some(".idea")
            | Some(".vscode")
    )
}

/// Register notify watches that skip high-volume ignore directories.
///
/// Strategy: a non-recursive watch on the root catches edits to top-level
/// files (Cargo.toml, README) and creation of new top-level entries; for each
/// existing top-level child *directory* not in the ignore set, a recursive
/// watch is added. New subdirectories created inside an already-recursively-
/// watched subtree are picked up automatically by the kernel.
///
/// Trade-off: a NEW top-level directory created after the watcher starts is
/// visible (CREATE event on root) but its contents are not live-watched until
/// the next `code_reindex` or server restart. Acceptable in practice — new
/// top-level dirs are rare; the cost of `target/` overflow is not.
fn watch_non_ignored(
    debouncer: &mut Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    root: &Path,
) -> Result<(), notify::Error> {
    debouncer.watch(root, RecursiveMode::NonRecursive)?;
    let entries = match std::fs::read_dir(root) {
        Ok(it) => it,
        Err(e) => {
            warn!(
                ?root,
                "watcher: read_dir failed, falling back to root-only watch: {e}"
            );
            return Ok(());
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || is_ignored_dir(&entry.file_name()) {
            continue;
        }
        debouncer.watch(&path, RecursiveMode::Recursive)?;
    }
    Ok(())
}

/// A file watcher that emits debounced change events.
pub struct Watcher {
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    rx: mpsc::Receiver<Vec<PathBuf>>,
}

impl Watcher {
    /// Start watching a directory with 250ms debounce.
    ///
    /// On notify overflow or generic errors, falls back to `gix status`
    /// to discover changed paths.
    pub fn new(root: &Path) -> Result<Self, notify::Error> {
        Self::new_with_debounce(root, Duration::from_millis(250))
    }

    /// Start watching a directory with a configurable debounce interval.
    pub fn new_with_debounce(root: &Path, debounce: Duration) -> Result<Self, notify::Error> {
        // Bounded channel: a backed-up consumer (slow parser, paused
        // debugger, etc.) blocks the debouncer's send rather than
        // letting the queue grow unbounded. 64 batches is generous —
        // each batch already coalesces 250 ms of edits, so 64 represents
        // ~16 s of continuous churn before the debouncer thread parks.
        let (tx, rx) = mpsc::sync_channel(64);

        let sender = tx.clone();
        let root_for_fallback = root.to_path_buf();
        let mut debouncer =
            new_debouncer(
                debounce,
                None,
                move |result: DebounceEventResult| match result {
                    Ok(events) => {
                        // Keep delete events: a deleted path is neither a file nor a
                        // directory, so `!p.is_dir()` lets it through (and excludes
                        // genuine directories). The downstream Phase 0 in the server
                        // checks `path.exists()` to discriminate delete vs. modify.
                        let paths: Vec<PathBuf> = events
                            .into_iter()
                            .flat_map(|e| e.event.paths)
                            .filter(|p| !p.components().any(|c| c.as_os_str() == ".recon"))
                            .filter(|p| !p.is_dir() && Language::from_path(p) != Language::Unknown)
                            .collect();
                        if !paths.is_empty() {
                            debug!(count = paths.len(), "debounced file changes");
                            let _ = sender.send(paths);
                        }
                    }
                    Err(errors) => {
                        // Detect any signal that events were lost or coalesced and
                        // fall back to gix status. notify-rs uses several phrasings
                        // across backends ("queue overflow", "Event queue has
                        // overflowed", "events lost", etc.); lowercase + a small
                        // substring set catches the realistic shapes without
                        // triggering on every transient error.
                        let is_event_loss = errors.iter().any(|e| {
                            let msg = format!("{e}").to_lowercase();
                            msg.contains("overflow")
                                || msg.contains("coalesced")
                                || msg.contains("lost")
                                || msg.contains("queue")
                        });

                        if is_event_loss {
                            warn!("notify event loss detected, falling back to gix status");
                            match crate::git::status_paths(&root_for_fallback) {
                                Ok(paths) => {
                                    let paths: Vec<PathBuf> = paths
                                        .into_iter()
                                        .filter(|p| {
                                            !p.components().any(|c| c.as_os_str() == ".recon")
                                        })
                                        .filter(|p| {
                                            !p.is_dir()
                                                && Language::from_path(p) != Language::Unknown
                                        })
                                        .collect();
                                    if !paths.is_empty() {
                                        let _ = sender.send(paths);
                                    }
                                }
                                Err(e) => {
                                    warn!("gix status fallback failed: {e}");
                                }
                            }
                        }

                        for e in &errors {
                            warn!("watch error: {e}");
                        }
                    }
                },
            )?;

        watch_non_ignored(&mut debouncer, root)?;
        drop(tx); // Drop our copy so rx drains when debouncer stops

        Ok(Self {
            _debouncer: debouncer,
            rx,
        })
    }

    /// Block until the next batch of changed paths arrives.
    pub fn recv(&self) -> Option<Vec<PathBuf>> {
        self.rx.recv().ok()
    }

    /// Block up to `timeout` for the next batch.
    ///
    /// Returns `Ok(paths)` on a batch, `Err(RecvTimeoutError::Timeout)` if
    /// nothing arrived in time, `Err(Disconnected)` once the debouncer stops.
    /// The timeout lets shutdown logic wake every few hundred ms to observe a
    /// cancellation flag.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Vec<PathBuf>, mpsc::RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    /// Non-blocking poll for changed paths.
    pub fn try_recv(&self) -> Option<Vec<PathBuf>> {
        self.rx.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::thread;
    use std::time::Duration;

    /// Helper: create a temp dir with a .git subdir so it looks like a repo.
    fn make_temp_root() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        tmp
    }

    #[test]
    fn watcher_new_on_valid_dir() {
        let tmp = make_temp_root();
        // Watcher::new should succeed on any readable directory
        let watcher = Watcher::new(tmp.path());
        assert!(
            watcher.is_ok(),
            "Watcher::new should succeed on a valid temp dir"
        );
    }

    #[test]
    fn watcher_new_on_nonexistent_dir_fails() {
        let result = Watcher::new(Path::new("/nonexistent/path/that/does/not/exist"));
        assert!(
            result.is_err(),
            "Watcher::new should fail on a nonexistent directory"
        );
    }

    #[test]
    fn watcher_detects_file_creation() {
        let tmp = make_temp_root();
        let watcher = Watcher::new(tmp.path()).unwrap();

        // Write a Rust file — the watcher should pick it up
        let file_path = tmp.path().join("hello.rs");
        fs::write(&file_path, "fn main() {}").unwrap();

        // Wait for the debounce window (250ms) plus a safety margin
        thread::sleep(Duration::from_millis(500));

        let events = watcher.try_recv();
        assert!(
            events.is_some(),
            "watcher should have detected the new .rs file"
        );
        let paths = events.unwrap();
        assert!(
            paths.iter().any(|p| p.ends_with("hello.rs")),
            "hello.rs should be in the event paths: {paths:?}"
        );
    }

    #[test]
    fn watcher_filters_recon_directory() {
        let tmp = make_temp_root();
        let recon_dir = tmp.path().join(".recon");
        fs::create_dir(&recon_dir).unwrap();

        let watcher = Watcher::new(tmp.path()).unwrap();

        // Write a file inside .recon — it should be filtered out
        let file_path = recon_dir.join("cache.db");
        fs::write(&file_path, b"fake cache data").unwrap();

        // Wait for debounce
        thread::sleep(Duration::from_millis(500));

        // try_recv may return None (filtered) or Some with no .recon paths
        if let Some(paths) = watcher.try_recv() {
            let has_recon = paths
                .iter()
                .any(|p| p.components().any(|c| c.as_os_str() == ".recon"));
            assert!(!has_recon, ".recon paths should be filtered out: {paths:?}");
        }
    }

    #[test]
    fn watcher_filters_non_source_files() {
        let tmp = make_temp_root();
        let watcher = Watcher::new(tmp.path()).unwrap();

        // Write a non-source file (e.g., .png) — should be filtered
        let file_path = tmp.path().join("image.png");
        fs::write(&file_path, b"\x89PNG\r\n\x1a\n").unwrap();

        thread::sleep(Duration::from_millis(500));

        if let Some(paths) = watcher.try_recv() {
            let has_png = paths.iter().any(|p| p.ends_with("image.png"));
            assert!(
                !has_png,
                "non-source files like .png should be filtered: {paths:?}"
            );
        }
    }

    #[test]
    fn watcher_detects_multiple_files() {
        let tmp = make_temp_root();
        let watcher = Watcher::new(tmp.path()).unwrap();

        // Write multiple Rust files
        fs::write(tmp.path().join("a.rs"), "fn a() {}").unwrap();
        fs::write(tmp.path().join("b.rs"), "fn b() {}").unwrap();
        fs::write(tmp.path().join("c.rs"), "fn c() {}").unwrap();

        thread::sleep(Duration::from_millis(500));

        let events = watcher.try_recv();
        assert!(
            events.is_some(),
            "watcher should have detected the new files"
        );
        let paths = events.unwrap();
        assert!(
            !paths.is_empty(),
            "should have at least 1 event path, got {}",
            paths.len()
        );
    }

    #[test]
    fn watcher_recv_blocks_until_event() {
        let tmp = make_temp_root();
        let watcher = Watcher::new(tmp.path()).unwrap();

        // Spawn a thread that writes a file after a short delay
        let root = tmp.path().to_path_buf();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            fs::write(root.join("delayed.rs"), "fn delayed() {}").unwrap();
        });

        // recv_timeout: macOS FSEvents in CI VMs can drop events for files
        // created under a NonRecursive root watch, leaving a plain `recv()`
        // hung forever. Bounded timeout fails fast instead of wedging the
        // whole test binary (cf. v0.2.2 release pipeline hang on macos-latest).
        let events = watcher.recv_timeout(Duration::from_secs(10));
        assert!(
            events.is_ok(),
            "recv_timeout should return events after file creation, got {events:?}"
        );

        handle.join().unwrap();
    }

    #[test]
    fn watcher_ignores_target_subdir() {
        let tmp = make_temp_root();
        // Pre-create target/ so the watcher's startup walk explicitly skips it.
        let target_dir = tmp.path().join("target");
        fs::create_dir(&target_dir).unwrap();

        let watcher = Watcher::new(tmp.path()).unwrap();

        // Drain any initial events.
        thread::sleep(Duration::from_millis(300));
        while watcher.try_recv().is_some() {}

        // Write a .rs file inside target/ — it would normally be a strong
        // signal, but target/ is ignored at the watch level so no event fires.
        fs::write(target_dir.join("build_artifact.rs"), "fn x() {}").unwrap();
        thread::sleep(Duration::from_millis(500));

        if let Some(paths) = watcher.try_recv() {
            let leaked = paths
                .iter()
                .any(|p| p.components().any(|c| c.as_os_str() == "target"));
            assert!(
                !leaked,
                "watcher should not emit events from target/: {paths:?}"
            );
        }

        // Sanity: a sibling file outside target/ still gets through.
        fs::write(tmp.path().join("real.rs"), "fn y() {}").unwrap();
        thread::sleep(Duration::from_millis(500));
        let paths = watcher.try_recv().expect("real.rs event should fire");
        assert!(paths.iter().any(|p| p.ends_with("real.rs")));
    }

    #[test]
    fn is_ignored_dir_covers_high_volume_paths() {
        for name in [
            "target",
            "node_modules",
            ".git",
            ".recon",
            ".idea",
            ".vscode",
        ] {
            assert!(
                is_ignored_dir(std::ffi::OsStr::new(name)),
                "{name} should be ignored"
            );
        }
        for name in ["src", "crates", "docs", "tests", "src.rs"] {
            assert!(
                !is_ignored_dir(std::ffi::OsStr::new(name)),
                "{name} should NOT be ignored"
            );
        }
    }

    #[test]
    fn overflow_substring_matches_realistic_phrasings() {
        // The watcher's Err branch lowercases and substring-matches against the
        // formatted error. These are the realistic phrasings we want to catch
        // — guard against future tightening of the regex.
        let phrasings = [
            "Event queue has overflowed",
            "queue overflow",
            "QOverflow",
            "events were lost",
            "Some events were coalesced",
        ];
        for s in phrasings {
            let lower = s.to_lowercase();
            let matched = lower.contains("overflow")
                || lower.contains("coalesced")
                || lower.contains("lost")
                || lower.contains("queue");
            assert!(matched, "should detect event-loss phrasing: {s:?}");
        }
    }

    #[test]
    fn watcher_emits_delete_events() {
        let tmp = make_temp_root();

        // Pre-create a Rust file so it exists when the watcher starts.
        let file_path = tmp.path().join("doomed.rs");
        fs::write(&file_path, "fn doomed() {}").unwrap();

        let watcher = Watcher::new(tmp.path()).unwrap();

        // Drain any startup events.
        thread::sleep(Duration::from_millis(300));
        while watcher.try_recv().is_some() {}

        // Delete the file — its path is neither a file nor a directory afterward,
        // but the watcher's filter (`!is_dir() && Language::from_path != Unknown`)
        // must still let the event through.
        fs::remove_file(&file_path).unwrap();
        thread::sleep(Duration::from_millis(500));

        let events = watcher.try_recv();
        assert!(
            events.is_some(),
            "watcher should emit an event for the delete"
        );
        let paths = events.unwrap();
        assert!(
            paths.iter().any(|p| p.ends_with("doomed.rs")),
            "deleted .rs file should be in event paths: {paths:?}"
        );
    }

    #[test]
    fn watcher_detects_file_modification() {
        let tmp = make_temp_root();

        // Pre-create a Rust file
        let file_path = tmp.path().join("existing.rs");
        fs::write(&file_path, "fn main() {}").unwrap();

        let watcher = Watcher::new(tmp.path()).unwrap();

        // Modify the file
        fs::write(&file_path, "fn main() { println!(\"hello\"); }").unwrap();

        thread::sleep(Duration::from_millis(500));

        let events = watcher.try_recv();
        assert!(
            events.is_some(),
            "watcher should have detected the file modification"
        );
        let paths = events.unwrap();
        assert!(
            paths.iter().any(|p| p.ends_with("existing.rs")),
            "existing.rs should be in the event paths: {paths:?}"
        );
    }
}
