//! Git integration via `gix` for incremental indexing.
//!
//! Provides HEAD resolution, worktree status, and changed-path detection
//! for ColdStart optimization and notify overflow fallback.

use recon_core::error::Error;
use std::path::{Path, PathBuf};

/// Paths changed in the working tree.
pub struct ChangedPaths {
    /// Files that are new or modified.
    pub modified: Vec<PathBuf>,
    /// Files that have been deleted.
    pub deleted: Vec<PathBuf>,
}

/// Get the HEAD commit SHA for a repo root.
///
/// Returns the full 40-character hex OID string.
pub fn head_sha(repo_root: &Path) -> Result<String, Error> {
    let repo = gix::open(repo_root).map_err(|e| Error::Storage(format!("gix open: {e}")))?;
    let id = repo
        .head_id()
        .map_err(|e| Error::Storage(format!("gix head_id: {e}")))?;
    Ok(id.to_hex().to_string())
}

/// Get paths with uncommitted changes via gix status.
///
/// Returns all modified, added, and deleted paths relative to the repo root.
/// Used as a notify overflow fallback and for incremental re-indexing.
pub fn status_paths(repo_root: &Path) -> Result<Vec<PathBuf>, Error> {
    let repo = gix::open(repo_root).map_err(|e| Error::Storage(format!("gix open: {e}")))?;

    let status = repo
        .status(gix::progress::Discard)
        .map_err(|e| Error::Storage(format!("gix status init: {e}")))?;

    let iter = status
        .into_iter(None::<gix::bstr::BString>)
        .map_err(|e| Error::Storage(format!("gix status iter: {e}")))?;

    let mut paths = Vec::new();
    for item in iter {
        let item = item.map_err(|e| Error::Storage(format!("gix status item: {e}")))?;
        let rela_path = match &item {
            gix::status::Item::IndexWorktree(iw) => Some(iw.rela_path().to_string()),
            gix::status::Item::TreeIndex(change) => {
                use gix::diff::index::Change;
                match change {
                    Change::Addition { location, .. }
                    | Change::Deletion { location, .. }
                    | Change::Modification { location, .. } => Some(location.to_string()),
                    Change::Rewrite {
                        source_location,
                        location,
                        ..
                    } => {
                        paths.push(repo_root.join(source_location.to_string()));
                        Some(location.to_string())
                    }
                }
            }
        };
        if let Some(p) = rela_path {
            paths.push(repo_root.join(p));
        }
    }
    Ok(paths)
}

/// Classify changed paths into modified vs deleted.
pub fn classify_changes(paths: &[PathBuf]) -> ChangedPaths {
    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    for p in paths {
        if p.exists() {
            modified.push(p.clone());
        } else {
            deleted.push(p.clone());
        }
    }
    ChangedPaths { modified, deleted }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_sha_on_this_repo() {
        // This project is a git repo, so head_sha should work
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let sha = head_sha(root).unwrap();
        assert_eq!(sha.len(), 40, "SHA should be 40 hex chars: {sha}");
        assert!(
            sha.chars().all(|c| c.is_ascii_hexdigit()),
            "SHA should be hex: {sha}"
        );
    }

    #[test]
    fn head_sha_non_git_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(head_sha(dir.path()).is_err());
    }

    #[test]
    fn status_paths_on_clean_repo() {
        // On a clean repo, status_paths should return an empty or small list
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        // This may or may not be empty depending on working tree state,
        // but it should not error
        let result = status_paths(root);
        assert!(result.is_ok(), "status_paths failed: {:?}", result.err());
    }

    #[test]
    fn classify_changes_splits_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("exists.rs");
        std::fs::write(&existing, "fn main() {}").unwrap();
        let missing = dir.path().join("gone.rs");

        let changes = classify_changes(&[existing.clone(), missing.clone()]);
        assert_eq!(changes.modified.len(), 1);
        assert_eq!(changes.deleted.len(), 1);
        assert_eq!(changes.modified[0], existing);
        assert_eq!(changes.deleted[0], missing);
    }
}
