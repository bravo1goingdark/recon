//! Git integration via `gix` for incremental indexing.
//!
//! Provides HEAD resolution, worktree status, and changed-path detection
//! for ColdStart optimization and notify overflow fallback.

use recon_core::error::Error;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Paths changed in the working tree.
pub struct ChangedPaths {
    /// Files that are new or modified.
    pub modified: Vec<PathBuf>,
    /// Files that have been deleted.
    pub deleted: Vec<PathBuf>,
}

/// Open a gix repository. Shared by all git operations.
pub fn open_repo(repo_root: &Path) -> Result<gix::Repository, Error> {
    gix::open(repo_root).map_err(|e| Error::Storage(format!("gix open: {e}")))
}

/// Convert a `&BStr` path to a `PathBuf` without an intermediate String allocation.
fn bstr_to_path(b: &gix::bstr::BStr) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(b.as_ref()))
}

/// Get the HEAD commit SHA for a repo root.
///
/// Returns the full 40-character hex OID string.
pub fn head_sha(repo_root: &Path) -> Result<String, Error> {
    head_sha_with_repo(&open_repo(repo_root)?)
}

/// Get HEAD SHA using an already-opened repository.
pub fn head_sha_with_repo(repo: &gix::Repository) -> Result<String, Error> {
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
    status_paths_with_repo(&open_repo(repo_root)?, repo_root)
}

/// Get status paths using an already-opened repository.
pub fn status_paths_with_repo(
    repo: &gix::Repository,
    repo_root: &Path,
) -> Result<Vec<PathBuf>, Error> {
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
            gix::status::Item::IndexWorktree(iw) => Some(bstr_to_path(iw.rela_path())),
            gix::status::Item::TreeIndex(change) => {
                use gix::diff::index::Change;
                match change {
                    Change::Addition { location, .. }
                    | Change::Deletion { location, .. }
                    | Change::Modification { location, .. } => Some(bstr_to_path(location)),
                    Change::Rewrite {
                        source_location,
                        location,
                        ..
                    } => {
                        paths.push(repo_root.join(bstr_to_path(source_location)));
                        Some(bstr_to_path(location))
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

/// Classify changed paths into modified vs deleted, consuming the input.
pub fn classify_changes(paths: Vec<PathBuf>) -> ChangedPaths {
    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    for p in paths {
        if p.exists() {
            modified.push(p);
        } else {
            deleted.push(p);
        }
    }
    ChangedPaths { modified, deleted }
}

/// Get uncommitted worktree changes classified as modified/deleted.
pub fn status_changed_paths(repo_root: &Path) -> Result<ChangedPaths, Error> {
    status_changed_paths_with_repo(&open_repo(repo_root)?, repo_root)
}

/// Get worktree changes using an already-opened repository.
pub fn status_changed_paths_with_repo(
    repo: &gix::Repository,
    repo_root: &Path,
) -> Result<ChangedPaths, Error> {
    let paths = status_paths_with_repo(repo, repo_root)?;
    Ok(classify_changes(paths))
}

/// Diff two commits' trees via gix, returning changed file paths.
///
/// Walks git's object store — **no working-tree file I/O**. Only blob
/// entries are reported (trees and submodules are skipped).
pub fn diff_commits(repo_root: &Path, old_sha: &str, new_sha: &str) -> Result<ChangedPaths, Error> {
    diff_commits_with_repo(&open_repo(repo_root)?, repo_root, old_sha, new_sha)
}

/// Diff two commits using an already-opened repository.
pub fn diff_commits_with_repo(
    repo: &gix::Repository,
    repo_root: &Path,
    old_sha: &str,
    new_sha: &str,
) -> Result<ChangedPaths, Error> {
    let old_id = gix::ObjectId::from_hex(old_sha.as_bytes())
        .map_err(|e| Error::Storage(format!("gix parse old sha: {e}")))?;
    let new_id = gix::ObjectId::from_hex(new_sha.as_bytes())
        .map_err(|e| Error::Storage(format!("gix parse new sha: {e}")))?;

    let old_tree = repo
        .find_object(old_id)
        .map_err(|e| Error::Storage(format!("gix find old: {e}")))?
        .try_into_commit()
        .map_err(|e| Error::Storage(format!("gix into_commit old: {e}")))?
        .tree()
        .map_err(|e| Error::Storage(format!("gix tree old: {e}")))?;
    let new_tree = repo
        .find_object(new_id)
        .map_err(|e| Error::Storage(format!("gix find new: {e}")))?
        .try_into_commit()
        .map_err(|e| Error::Storage(format!("gix into_commit new: {e}")))?
        .tree()
        .map_err(|e| Error::Storage(format!("gix tree new: {e}")))?;

    let mut modified = Vec::new();
    let mut deleted = Vec::new();

    old_tree
        .changes()
        .map_err(|e| Error::Storage(format!("gix changes init: {e}")))?
        .for_each_to_obtain_tree(&new_tree, |change| {
            use gix::object::tree::diff::Change;
            match change {
                Change::Addition {
                    location,
                    entry_mode,
                    ..
                }
                | Change::Modification {
                    location,
                    entry_mode,
                    ..
                } => {
                    if entry_mode.is_blob() {
                        modified.push(repo_root.join(bstr_to_path(location)));
                    }
                }
                Change::Deletion {
                    location,
                    entry_mode,
                    ..
                } => {
                    if entry_mode.is_blob() {
                        deleted.push(repo_root.join(bstr_to_path(location)));
                    }
                }
                Change::Rewrite {
                    source_location,
                    location,
                    ..
                } => {
                    deleted.push(repo_root.join(bstr_to_path(source_location)));
                    modified.push(repo_root.join(bstr_to_path(location)));
                }
            }
            Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()))
        })
        .map_err(|e| Error::Storage(format!("gix tree diff: {e}")))?;

    Ok(ChangedPaths { modified, deleted })
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

        let changes = classify_changes(vec![existing.clone(), missing.clone()]);
        assert_eq!(changes.modified.len(), 1);
        assert_eq!(changes.deleted.len(), 1);
        assert_eq!(changes.modified[0], existing);
        assert_eq!(changes.deleted[0], missing);
    }

    /// Helper: run a git command in a directory.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Helper: init a temp git repo with initial commit, return (dir, sha).
    fn init_repo() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init"]);
        git(dir.path(), &["config", "user.email", "test@test.com"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "init"]);
        let sha = head_sha(dir.path()).unwrap();
        (dir, sha)
    }

    #[test]
    fn diff_commits_detects_changes() {
        let (dir, sha1) = init_repo();

        // Modify main.rs, add new.rs, delete nothing yet
        std::fs::write(dir.path().join("main.rs"), "fn main() { println!(); }").unwrap();
        std::fs::write(dir.path().join("new.rs"), "pub fn new() {}").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "second"]);
        let sha2 = head_sha(dir.path()).unwrap();

        let diff = diff_commits(dir.path(), &sha1, &sha2).unwrap();

        let mod_names: Vec<String> = diff
            .modified
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(
            mod_names.contains(&"main.rs".to_string()),
            "should detect modified main.rs: {mod_names:?}"
        );
        assert!(
            mod_names.contains(&"new.rs".to_string()),
            "should detect added new.rs: {mod_names:?}"
        );
        assert!(diff.deleted.is_empty(), "nothing was deleted");
    }

    #[test]
    fn diff_commits_detects_deletion() {
        let (dir, _) = init_repo();

        // Add a file, commit, then delete it
        std::fs::write(dir.path().join("extra.rs"), "pub fn extra() {}").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "add extra"]);
        let sha2 = head_sha(dir.path()).unwrap();

        std::fs::remove_file(dir.path().join("extra.rs")).unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "delete extra"]);
        let sha3 = head_sha(dir.path()).unwrap();

        let diff = diff_commits(dir.path(), &sha2, &sha3).unwrap();
        let del_names: Vec<String> = diff
            .deleted
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(
            del_names.contains(&"extra.rs".to_string()),
            "should detect deleted extra.rs: {del_names:?}"
        );
    }

    #[test]
    fn diff_commits_same_commit_no_changes() {
        let (dir, sha) = init_repo();

        let diff = diff_commits(dir.path(), &sha, &sha).unwrap();
        assert!(diff.modified.is_empty(), "same commit should have no mods");
        assert!(
            diff.deleted.is_empty(),
            "same commit should have no deletes"
        );
    }

    #[test]
    fn diff_commits_handles_rename() {
        let (dir, sha1) = init_repo();

        git(dir.path(), &["mv", "main.rs", "app.rs"]);
        git(dir.path(), &["commit", "-m", "rename"]);
        let sha2 = head_sha(dir.path()).unwrap();

        let diff = diff_commits(dir.path(), &sha1, &sha2).unwrap();

        // Rename may show as delete old + add new, or as a Rewrite
        // Either way, app.rs should be in modified and main.rs should be in deleted
        let mod_names: Vec<String> = diff
            .modified
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        let del_names: Vec<String> = diff
            .deleted
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(
            mod_names.contains(&"app.rs".to_string()),
            "renamed destination should appear in modified: {mod_names:?}"
        );
        assert!(
            del_names.contains(&"main.rs".to_string()),
            "renamed source should appear in deleted: {del_names:?}"
        );
    }
}
