//! File watcher using notify with debouncing.
//!
//! On notify overflow errors, falls back to `gix status` to discover
//! changed paths instead of losing events silently.

use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use recon_core::lang::Language;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use tracing::{debug, warn};

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
        let (tx, rx) = mpsc::channel();

        let sender = tx.clone();
        let root_for_fallback = root.to_path_buf();
        let mut debouncer = new_debouncer(
            Duration::from_millis(250),
            None,
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    let paths: Vec<PathBuf> = events
                        .into_iter()
                        .flat_map(|e| e.event.paths)
                        .filter(|p| !p.components().any(|c| c.as_os_str() == ".recon"))
                        .filter(|p| p.is_file() && Language::from_path(p) != Language::Unknown)
                        .collect();
                    if !paths.is_empty() {
                        debug!(count = paths.len(), "debounced file changes");
                        let _ = sender.send(paths);
                    }
                }
                Err(errors) => {
                    // Detect overflow / coalescing errors and fall back to gix status
                    let is_overflow = errors.iter().any(|e| {
                        let msg = format!("{e}");
                        msg.contains("overflow") || msg.contains("coalesced")
                    });

                    if is_overflow {
                        warn!("notify overflow, falling back to gix status");
                        match crate::git::status_paths(&root_for_fallback) {
                            Ok(paths) => {
                                let paths: Vec<PathBuf> = paths
                                    .into_iter()
                                    .filter(|p| !p.components().any(|c| c.as_os_str() == ".recon"))
                                    .filter(|p| {
                                        p.is_file() && Language::from_path(p) != Language::Unknown
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

        debouncer.watch(root, RecursiveMode::Recursive)?;
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

        // recv() should block and then return when the file is written
        let events = watcher.recv();
        assert!(
            events.is_some(),
            "recv() should return events after file creation"
        );

        handle.join().unwrap();
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
