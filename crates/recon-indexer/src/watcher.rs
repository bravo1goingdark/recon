//! File watcher using notify with debouncing.

use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use notify::RecursiveMode;
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
    pub fn new(root: &Path) -> Result<Self, notify::Error> {
        let (tx, rx) = mpsc::channel();

        let sender = tx.clone();
        let mut debouncer = new_debouncer(
            Duration::from_millis(250),
            None,
            move |result: DebounceEventResult| {
                match result {
                    Ok(events) => {
                        let paths: Vec<PathBuf> = events
                            .into_iter()
                            .flat_map(|e| e.event.paths)
                            .filter(|p| {
                                p.is_file() && Language::from_path(p) != Language::Unknown
                            })
                            .collect();
                        if !paths.is_empty() {
                            debug!(count = paths.len(), "debounced file changes");
                            let _ = sender.send(paths);
                        }
                    }
                    Err(errors) => {
                        for e in errors {
                            warn!("watch error: {e}");
                        }
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

    /// Non-blocking poll for changed paths.
    pub fn try_recv(&self) -> Option<Vec<PathBuf>> {
        self.rx.try_recv().ok()
    }
}
