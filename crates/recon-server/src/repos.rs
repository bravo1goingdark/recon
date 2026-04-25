//! Global repo tracking for recon.
//!
//! Records which repositories have been indexed so that the `max_repos` license
//! limit can be enforced across multiple projects on the same machine.
//!
//! The registry lives at `<config_dir>/repos.json` (typically
//! `~/.config/recon/repos.json`).  It is a plain JSON array of [`RepoEntry`]
//! objects — no database, no migrations, easy to inspect or repair manually.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

// ── Data types ─────────────────────────────────────────────────────────────────

/// A record of a single indexed repository.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoEntry {
    /// Canonicalized absolute path to the repository root.
    pub path: String,
    /// Unix timestamp (seconds) of the last successful index.
    pub indexed_at: u64,
    /// Number of source files indexed.
    pub files: usize,
    /// Number of symbols indexed.
    pub symbols: usize,
}

// ── I/O helpers ────────────────────────────────────────────────────────────────

/// Load the tracked repos list from `<config_dir>/repos.json`.
///
/// Returns an empty `Vec` if the file does not exist yet.  Any I/O or
/// deserialisation error is returned as a `String`.
pub fn load_repos(config_dir: &Path) -> Result<Vec<RepoEntry>, String> {
    let path = config_dir.join("repos.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
}

/// Persist the tracked repos list to `<config_dir>/repos.json`.
pub fn save_repos(config_dir: &Path, repos: &[RepoEntry]) -> Result<(), String> {
    let path = config_dir.join("repos.json");
    let json = serde_json::to_string_pretty(repos).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())
}

// ── Query helpers ──────────────────────────────────────────────────────────────

/// Returns `true` if `path` (canonicalized) is already in the registry.
pub fn is_indexed(repos: &[RepoEntry], path: &str) -> bool {
    repos.iter().any(|r| r.path == path)
}

// ── Mutation helpers ───────────────────────────────────────────────────────────

/// Register or update a repo, then write the registry to disk.
///
/// If an entry for `path` already exists it is updated in-place; otherwise a
/// new entry is appended.  The caller must supply the canonicalized repo path.
pub fn add_or_update_repo(
    config_dir: &Path,
    path: &str,
    files: usize,
    symbols: usize,
) -> Result<(), String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut repos = load_repos(config_dir)?;
    if let Some(entry) = repos.iter_mut().find(|r| r.path == path) {
        entry.indexed_at = now;
        entry.files = files;
        entry.symbols = symbols;
    } else {
        repos.push(RepoEntry {
            path: path.to_string(),
            indexed_at: now,
            files,
            symbols,
        });
    }
    save_repos(config_dir, &repos)
}

/// Remove a repo from the registry by canonical path.
///
/// Returns `Ok(true)` if an entry was removed, `Ok(false)` if `path` was not
/// in the registry. Used by `recon purge --mcp <ide>` to free a license slot
/// when the user tears down recon's wiring in a project.
pub fn remove_repo(config_dir: &Path, path: &str) -> Result<bool, String> {
    let mut repos = load_repos(config_dir)?;
    let before = repos.len();
    repos.retain(|r| r.path != path);
    if repos.len() == before {
        return Ok(false);
    }
    save_repos(config_dir, &repos)?;
    Ok(true)
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    // ── load_repos ─────────────────────────────────────────────────────────────

    #[test]
    fn load_repos_missing_file_returns_empty() {
        let dir = tempdir().unwrap();
        let repos = load_repos(dir.path()).unwrap();
        assert!(repos.is_empty());
    }

    #[test]
    fn load_repos_empty_array_returns_empty() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("repos.json"), "[]").unwrap();
        let repos = load_repos(dir.path()).unwrap();
        assert!(repos.is_empty());
    }

    #[test]
    fn load_repos_corrupt_json_returns_err() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("repos.json"), "not json {{").unwrap();
        assert!(load_repos(dir.path()).is_err());
    }

    // ── save_repos / roundtrip ─────────────────────────────────────────────────

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let entries = vec![
            RepoEntry {
                path: "/home/user/project-a".into(),
                indexed_at: 1_000_000,
                files: 100,
                symbols: 500,
            },
            RepoEntry {
                path: "/home/user/project-b".into(),
                indexed_at: 2_000_000,
                files: 200,
                symbols: 1_000,
            },
        ];
        save_repos(dir.path(), &entries).unwrap();
        let loaded = load_repos(dir.path()).unwrap();
        assert_eq!(loaded, entries);
    }

    #[test]
    fn save_repos_writes_valid_json() {
        let dir = tempdir().unwrap();
        save_repos(dir.path(), &[]).unwrap();
        let content = std::fs::read_to_string(dir.path().join("repos.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(v.is_array());
    }

    // ── is_indexed ─────────────────────────────────────────────────────────────

    #[test]
    fn is_indexed_returns_true_for_known_path() {
        let repos = vec![RepoEntry {
            path: "/repo/a".into(),
            indexed_at: now(),
            files: 10,
            symbols: 50,
        }];
        assert!(is_indexed(&repos, "/repo/a"));
    }

    #[test]
    fn is_indexed_returns_false_for_unknown_path() {
        let repos = vec![RepoEntry {
            path: "/repo/a".into(),
            indexed_at: now(),
            files: 10,
            symbols: 50,
        }];
        assert!(!is_indexed(&repos, "/repo/b"));
    }

    #[test]
    fn is_indexed_empty_list_returns_false() {
        assert!(!is_indexed(&[], "/any/path"));
    }

    // ── add_or_update_repo ─────────────────────────────────────────────────────

    #[test]
    fn add_new_repo_creates_entry() {
        let dir = tempdir().unwrap();
        add_or_update_repo(dir.path(), "/repo/a", 42, 100).unwrap();
        let repos = load_repos(dir.path()).unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].path, "/repo/a");
        assert_eq!(repos[0].files, 42);
        assert_eq!(repos[0].symbols, 100);
    }

    #[test]
    fn add_repo_twice_does_not_duplicate() {
        let dir = tempdir().unwrap();
        add_or_update_repo(dir.path(), "/repo/a", 10, 50).unwrap();
        add_or_update_repo(dir.path(), "/repo/a", 20, 100).unwrap();
        let repos = load_repos(dir.path()).unwrap();
        assert_eq!(repos.len(), 1, "same path must not be duplicated");
        assert_eq!(repos[0].files, 20, "stats must be updated");
        assert_eq!(repos[0].symbols, 100);
    }

    #[test]
    fn add_multiple_distinct_repos() {
        let dir = tempdir().unwrap();
        add_or_update_repo(dir.path(), "/repo/a", 10, 50).unwrap();
        add_or_update_repo(dir.path(), "/repo/b", 20, 100).unwrap();
        add_or_update_repo(dir.path(), "/repo/c", 30, 150).unwrap();
        let repos = load_repos(dir.path()).unwrap();
        assert_eq!(repos.len(), 3);
    }

    // ── remove_repo ────────────────────────────────────────────────────────────

    #[test]
    fn remove_existing_repo_returns_true() {
        let dir = tempdir().unwrap();
        add_or_update_repo(dir.path(), "/repo/a", 10, 50).unwrap();
        add_or_update_repo(dir.path(), "/repo/b", 20, 100).unwrap();
        assert!(remove_repo(dir.path(), "/repo/a").unwrap());
        let repos = load_repos(dir.path()).unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].path, "/repo/b");
    }

    #[test]
    fn remove_unknown_repo_returns_false() {
        let dir = tempdir().unwrap();
        add_or_update_repo(dir.path(), "/repo/a", 10, 50).unwrap();
        assert!(!remove_repo(dir.path(), "/repo/missing").unwrap());
        assert_eq!(load_repos(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn remove_repo_on_missing_registry_returns_false() {
        let dir = tempdir().unwrap();
        assert!(!remove_repo(dir.path(), "/repo/a").unwrap());
    }

    #[test]
    fn update_existing_repo_timestamps_change() {
        let dir = tempdir().unwrap();
        // Seed with an ancient timestamp.
        save_repos(
            dir.path(),
            &[RepoEntry {
                path: "/repo/a".into(),
                indexed_at: 1,
                files: 5,
                symbols: 10,
            }],
        )
        .unwrap();
        let before = now();
        add_or_update_repo(dir.path(), "/repo/a", 99, 200).unwrap();
        let repos = load_repos(dir.path()).unwrap();
        assert!(
            repos[0].indexed_at >= before,
            "indexed_at must be refreshed"
        );
    }
}
