//! Multi-repo router with DashMap for lock-free concurrent access.
//!
//! Supports Free and Pro tiers with configurable repo limits.
//! Pro tier supports subscription expiry — repos are served until expiry,
//! then cleaned up to free memory. No surprise LRU eviction for paying users.

use crate::server::ReconServer;
use dashmap::DashMap;
use recon_core::config::Config;
use recon_indexer::indexer;
use recon_search::tantivy_backend::TantivyBackend;
use recon_storage::store::Store;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing::{info, warn};

/// Router-level errors.
#[derive(Debug, Error)]
pub enum RouterError {
    /// Subscription has expired.
    #[error("subscription expired — renew to continue using Pro features")]
    Expired,

    /// Failed to canonicalize a path.
    #[error("canonicalize: {0}")]
    Canonicalize(#[source] std::io::Error),

    /// Repo limit reached for the current tier.
    #[error("repo limit reached ({0} max for {1} tier)")]
    RepoLimit(usize, String),

    /// Repository has too many source files.
    #[error("Repository has {0} source files — exceeds the {1} tier limit of {2} files. Upgrade to a higher tier for larger repositories.")]
    FileLimit(usize, String, usize),

    /// Repository has too many lines of code.
    #[error("Repository has approximately {0}K lines of code — exceeds the {1} tier limit of {2}K LOC. Upgrade to a higher tier for larger repositories.")]
    LocLimit(usize, String, usize),

    /// Storage backend error.
    #[error("storage: {0}")]
    Storage(String),

    /// Search backend error.
    #[error("search: {0}")]
    Search(String),

    /// Core library error.
    #[error("core: {0}")]
    Core(#[from] recon_core::error::Error),
}

/// Global monotonic counter for access ordering.
static ACCESS_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Resource limits for a tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierLimits {
    /// Maximum number of concurrently loaded repos.
    pub max_repos: usize,
    /// Maximum source files per repo (checked before indexing).
    pub max_files: usize,
    /// Approximate maximum lines of code per repo.
    pub max_loc: usize,
}

/// Built-in tier presets. Add new tiers here — no match arms to update.
impl TierLimits {
    /// Starter (free): 1 repo, 250 files, 10K LOC.
    pub const FREE: Self = Self {
        max_repos: 1,
        max_files: 250,
        max_loc: 10_000,
    };

    /// Pro: 10 repos, 5K files, 200K LOC.
    pub const PRO: Self = Self {
        max_repos: 10,
        max_files: 5_000,
        max_loc: 200_000,
    };

    /// Team: 25 repos, 50K files, 4M LOC.
    pub const TEAM: Self = Self {
        max_repos: 25,
        max_files: 50_000,
        max_loc: 4_000_000,
    };

    /// Enterprise: 1000 repos, unlimited files/LOC.
    pub const ENTERPRISE: Self = Self {
        max_repos: 1_000,
        max_files: usize::MAX,
        max_loc: usize::MAX,
    };

    /// Uncapped (self-hosted, no limits).
    pub const UNCAPPED: Self = Self {
        max_repos: usize::MAX,
        max_files: usize::MAX,
        max_loc: usize::MAX,
    };
}

/// Subscription tier — a name + resource limits.
///
/// Not an enum: adding a new tier is just a new `TierLimits` constant and
/// a name in the `FromStr` parser. No match arms to update anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier {
    /// Tier name for error messages and logging.
    name: &'static str,
    /// Resource limits.
    limits: TierLimits,
}

impl Tier {
    /// Create a tier with a name and limits.
    pub const fn new(name: &'static str, limits: TierLimits) -> Self {
        Self { name, limits }
    }

    /// Get the resource limits.
    pub fn limits(self) -> TierLimits {
        self.limits
    }

    /// Maximum number of repos allowed.
    pub fn max_repos(self) -> usize {
        self.limits.max_repos
    }

    /// Maximum source files per repo.
    pub fn max_files(self) -> usize {
        self.limits.max_files
    }

    /// Maximum lines of code per repo.
    pub fn max_loc(self) -> usize {
        self.limits.max_loc
    }

    /// Human-readable tier name for error messages.
    pub fn name(self) -> &'static str {
        self.name
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.limits.max_files == usize::MAX {
            write!(
                f,
                "{} (max {} repos, unlimited)",
                self.name, self.limits.max_repos
            )
        } else {
            write!(
                f,
                "{} (max {} repos, {} files, {}K LOC)",
                self.name,
                self.limits.max_repos,
                self.limits.max_files,
                self.limits.max_loc / 1000,
            )
        }
    }
}

impl std::str::FromStr for Tier {
    type Err = String;

    /// Parse tier from CLI.
    ///
    /// Format: `TIER[:REPOS[:FILES[:LOC]]]`
    ///
    /// Supported tiers: `free`, `pro`, `team`, `enterprise`, `uncapped`.
    /// Omitted numeric fields use the tier's built-in defaults.
    ///
    /// Examples:
    /// - `free`                     → 1 repo, 2K files, 200K LOC
    /// - `pro:100`                  → 100 repos, 20K files, 2M LOC
    /// - `team`                     → 200 repos, 50K files, 5M LOC
    /// - `enterprise`               → 1000 repos, unlimited
    /// - `free:3:5000:500000`       → 3 repos, 5K files, 500K LOC
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.to_lowercase();
        let parts: Vec<&str> = lower.splitn(2, ':').collect();
        let tier_name = parts[0];

        let (name, defaults): (&'static str, TierLimits) = match tier_name {
            "free" => ("Free", TierLimits::FREE),
            "pro" => ("Pro", TierLimits::PRO),
            "team" => ("Team", TierLimits::TEAM),
            "enterprise" => ("Enterprise", TierLimits::ENTERPRISE),
            "uncapped" => ("Uncapped", TierLimits::UNCAPPED),
            _ => {
                return Err(format!(
                    "unknown tier '{tier_name}': expected one of: free, pro, team, enterprise, uncapped"
                ))
            }
        };

        let limits = if parts.len() == 1 {
            defaults
        } else {
            let nums: Vec<&str> = parts[1].split(':').collect();
            let max_repos = if !nums.is_empty() && !nums[0].is_empty() {
                nums[0]
                    .parse::<usize>()
                    .map_err(|e| format!("invalid max_repos: {e}"))?
            } else {
                defaults.max_repos
            };
            let max_files = if nums.len() > 1 && !nums[1].is_empty() {
                nums[1]
                    .parse::<usize>()
                    .map_err(|e| format!("invalid max_files: {e}"))?
            } else {
                defaults.max_files
            };
            let max_loc = if nums.len() > 2 && !nums[2].is_empty() {
                nums[2]
                    .parse::<usize>()
                    .map_err(|e| format!("invalid max_loc: {e}"))?
            } else {
                defaults.max_loc
            };
            TierLimits {
                max_repos,
                max_files,
                max_loc,
            }
        };

        Ok(Tier::new(name, limits))
    }
}

/// Per-repo state held in the router.
pub struct RepoState {
    /// The MCP server instance for this repo.
    pub server: ReconServer,
    /// Monotonic access counter for ordering.
    last_accessed: AtomicU64,
}

impl RepoState {
    fn new(server: ReconServer) -> Self {
        Self {
            server,
            last_accessed: AtomicU64::new(ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed)),
        }
    }

    /// Update the access counter (called on every request).
    pub fn touch(&self) {
        self.last_accessed.store(
            ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }

    /// Get the last access counter value.
    pub fn last_accessed(&self) -> u64 {
        self.last_accessed.load(Ordering::Relaxed)
    }
}

/// Multi-repo manager backed by DashMap for lock-free concurrent access.
///
/// Uses `DashMap::entry()` for atomic get-or-insert to prevent double-indexing
/// when two requests for the same repo arrive simultaneously (TOCTOU fix).
///
/// Lifecycle:
/// - Both tiers **reject** at their repo limit (no surprise eviction).
/// - Pro tier supports expiry: call `set_expires_at()` with a unix timestamp.
///   After expiry, `get_or_load()` rejects new requests, and `sweep_expired()`
///   unloads all repos to reclaim memory.
pub struct RepoRouter {
    repos: DashMap<PathBuf, Arc<RepoState>>,
    tier: Tier,
    /// Unix timestamp when the subscription expires (0 = no expiry / free tier).
    expires_at: AtomicU64,
}

impl RepoRouter {
    /// Create a new router with the given tier limits.
    pub fn new(tier: Tier) -> Self {
        Self {
            repos: DashMap::new(),
            tier,
            expires_at: AtomicU64::new(0), // 0 = no expiry
        }
    }

    /// Set subscription expiry as a unix timestamp (seconds since epoch).
    ///
    /// After this time, `get_or_load()` rejects new requests and
    /// `sweep_expired()` cleans up loaded repos.
    pub fn set_expires_at(&self, unix_secs: u64) {
        self.expires_at.store(unix_secs, Ordering::Relaxed);
        info!(expires_at = unix_secs, "subscription expiry set");
    }

    /// Check if the subscription has expired.
    pub fn is_expired(&self) -> bool {
        let exp = self.expires_at.load(Ordering::Relaxed);
        if exp == 0 {
            return false; // No expiry set (free tier or indefinite)
        }
        now_epoch_secs() > exp
    }

    /// Get or lazily load a ReconServer for the given repo path.
    ///
    /// Returns an error if:
    /// - Subscription has expired
    /// - Repo limit reached for the current tier
    pub fn get_or_load(&self, repo_path: &Path) -> Result<ReconServer, RouterError> {
        if self.is_expired() {
            return Err(RouterError::Expired);
        }

        let repo_path = repo_path
            .canonicalize()
            .map_err(RouterError::Canonicalize)?;

        // Check tier limit before loading
        if !self.repos.contains_key(&repo_path) && self.repos.len() >= self.tier.max_repos() {
            return Err(RouterError::RepoLimit(
                self.tier.max_repos(),
                self.tier.name().to_string(),
            ));
        }

        // Atomic get-or-insert via entry API — prevents TOCTOU double-indexing
        let entry = self.repos.entry(repo_path.clone());
        match entry {
            dashmap::mapref::entry::Entry::Occupied(e) => {
                let state = e.get().clone();
                state.touch();
                Ok(state.server.clone())
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                let server = Self::load_repo(&repo_path, self.tier)?;
                let state = Arc::new(RepoState::new(server.clone()));
                e.insert(state);
                info!(repo = %repo_path.display(), "loaded repo");
                Ok(server)
            }
        }
    }

    /// Unload all repos after subscription expiry to reclaim memory.
    ///
    /// Safe to call periodically (e.g. from a background timer). Does nothing
    /// if the subscription is still active. In-flight requests holding `Arc`
    /// clones will finish naturally; memory is freed when the last clone drops.
    pub fn sweep_expired(&self) -> usize {
        if !self.is_expired() {
            return 0;
        }
        let count = self.repos.len();
        if count > 0 {
            self.repos.clear();
            info!(
                repos_freed = count,
                "swept expired repos — memory reclaimed"
            );
        }
        count
    }

    /// Number of currently loaded repos.
    pub fn repo_count(&self) -> usize {
        self.repos.len()
    }

    /// List all loaded repo paths.
    pub fn loaded_repos(&self) -> Vec<PathBuf> {
        self.repos.iter().map(|r| r.key().clone()).collect()
    }

    /// List loaded repos with their cached `(file_count, symbol_count)`
    /// tuples. Reads lock-free from each `ReconServer`'s atomic caches —
    /// no SQL queries, no allocations beyond the result `Vec`.
    pub fn loaded_repos_with_stats(&self) -> Vec<(PathBuf, u64, u64)> {
        self.repos
            .iter()
            .map(|r| {
                let server = &r.value().server;
                (r.key().clone(), server.file_count(), server.symbol_count())
            })
            .collect()
    }

    /// Explicitly unload a repo (e.g. on user request).
    pub fn unload(&self, repo_path: &Path) -> bool {
        self.repos.remove(repo_path).is_some()
    }

    /// Get the current tier.
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Upgrade the tier at runtime (e.g. user upgraded from Free to Pro).
    pub fn set_tier(&mut self, tier: Tier) {
        info!(old = %self.tier, new = %tier, "tier changed");
        self.tier = tier;
    }

    /// Load and index a single repo, enforcing tier limits on repo size.
    ///
    /// Walks the repo first to count files and estimate LOC. If the repo
    /// exceeds the tier limit, returns a clear error with upgrade guidance.
    fn load_repo(repo_path: &Path, tier: Tier) -> Result<ReconServer, RouterError> {
        let limits = tier.limits();
        let tier_name = tier.name();

        // Pre-flight: walk the repo and check file count before any indexing
        let config = Config::load(repo_path);
        let index_options = indexer::IndexOptions {
            max_file_size: config.max_file_size,
            tantivy_heap_bytes: config.tantivy_heap_bytes,
            allow_sensitive: config.allow_sensitive,
            ignore_patterns: config.ignore_patterns.clone(),
        };
        let paths: Vec<_> = recon_indexer::walker::walk_repo_with_ignores(
            repo_path,
            config.max_file_size,
            &config.ignore_patterns,
        )
        .into_iter()
        .filter(|p| {
            config.allow_sensitive
                || !recon_core::redact::is_blocked_path(p.strip_prefix(repo_path).unwrap_or(p))
        })
        .collect();
        let file_count = paths.len();

        if file_count > limits.max_files {
            return Err(RouterError::FileLimit(
                file_count,
                tier_name.to_string(),
                limits.max_files,
            ));
        }

        // Estimate LOC by sampling first 200 files, extrapolate to full repo
        let sample_size = file_count.min(200);
        if sample_size > 0 {
            let sample_loc: usize = paths[..sample_size]
                .iter()
                .filter_map(|p| std::fs::read(p).ok())
                .map(|c| c.iter().filter(|&&b| b == b'\n').count())
                .sum();
            let estimated_loc =
                (sample_loc as f64 / sample_size as f64 * file_count as f64) as usize;

            if estimated_loc > limits.max_loc {
                return Err(RouterError::LocLimit(
                    estimated_loc / 1000,
                    tier_name.to_string(),
                    limits.max_loc / 1000,
                ));
            }

            info!(
                repo = %repo_path.display(),
                files = file_count,
                estimated_loc,
                "repo within {} tier limits",
                tier_name,
            );
        }

        let store_dir = repo_path.join(".recon");
        std::fs::create_dir_all(&store_dir).map_err(|e| RouterError::Storage(e.to_string()))?;

        let store = Store::open(&store_dir.join("index.db"))
            .map_err(|e| RouterError::Storage(e.to_string()))?;
        let tantivy = TantivyBackend::open(&store_dir.join("tantivy"))
            .map_err(|e| RouterError::Search(e.to_string()))?;

        let mut writer = match tantivy.writer(config.tantivy_heap_bytes) {
            Ok(w) => Some(w),
            Err(e) => {
                warn!(
                    repo = %repo_path.display(),
                    %e,
                    "tantivy writer creation failed; BM25 indexing skipped for this repo \
                     (most often a stale .tantivy-writer.lock from a previously killed process)"
                );
                None
            }
        };
        match indexer::index_repo_incremental_with_options(
            &store,
            Some(&tantivy),
            repo_path,
            writer.as_mut(),
            index_options,
        ) {
            Ok(stats) => {
                info!(
                    repo = %repo_path.display(),
                    files = stats.files_indexed,
                    symbols = stats.total_symbols,
                    "indexed repo"
                );
            }
            Err(e) => {
                warn!(repo = %repo_path.display(), "index error: {e}");
            }
        }

        let server = ReconServer::new(repo_path.to_path_buf(), store, tantivy)?;
        if let Err(e) = server.init_embed() {
            warn!(repo = %repo_path.display(), "embed init failed, semantic search disabled: {e}");
        }
        Ok(server)
    }
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir)
            .output()
            .ok();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .ok();
        std::process::Command::new("git")
            .args(["commit", "-m", "init", "--quiet"])
            .current_dir(dir)
            .output()
            .ok();
    }

    #[test]
    fn tier_free_limits_to_one_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo1 = dir.path().join("repo1");
        let repo2 = dir.path().join("repo2");
        make_test_repo(&repo1);
        make_test_repo(&repo2);

        let router = RepoRouter::new(Tier::new("Free", TierLimits::FREE));
        assert!(router.get_or_load(&repo1).is_ok());
        assert_eq!(router.repo_count(), 1);

        // Second repo should fail (Free = 1 max, no eviction)
        let result = router.get_or_load(&repo2);
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("repo limit reached"),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn tier_free_configurable_limit() {
        let dir = tempfile::tempdir().unwrap();
        let repo1 = dir.path().join("repo1");
        let repo2 = dir.path().join("repo2");
        let repo3 = dir.path().join("repo3");
        make_test_repo(&repo1);
        make_test_repo(&repo2);
        make_test_repo(&repo3);

        // Free tier with 2 repos allowed
        let router = RepoRouter::new(Tier::new(
            "Free",
            TierLimits {
                max_repos: 2,
                ..TierLimits::FREE
            },
        ));
        assert!(router.get_or_load(&repo1).is_ok());
        assert!(router.get_or_load(&repo2).is_ok());
        assert_eq!(router.repo_count(), 2);

        // Third repo should fail
        assert!(router.get_or_load(&repo3).is_err());
    }

    #[test]
    fn tier_pro_allows_multiple() {
        let dir = tempfile::tempdir().unwrap();
        let repo1 = dir.path().join("repo1");
        let repo2 = dir.path().join("repo2");
        make_test_repo(&repo1);
        make_test_repo(&repo2);

        let router = RepoRouter::new(Tier::new(
            "Pro",
            TierLimits {
                max_repos: 10,
                ..TierLimits::PRO
            },
        ));
        assert!(router.get_or_load(&repo1).is_ok());
        assert!(router.get_or_load(&repo2).is_ok());
        assert_eq!(router.repo_count(), 2);
    }

    #[test]
    fn pro_rejects_at_limit_no_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let repo1 = dir.path().join("repo1");
        let repo2 = dir.path().join("repo2");
        let repo3 = dir.path().join("repo3");
        make_test_repo(&repo1);
        make_test_repo(&repo2);
        make_test_repo(&repo3);

        // Pro with limit of 2 — should reject 3rd, not evict
        let router = RepoRouter::new(Tier::new(
            "Pro",
            TierLimits {
                max_repos: 2,
                ..TierLimits::PRO
            },
        ));
        assert!(router.get_or_load(&repo1).is_ok());
        assert!(router.get_or_load(&repo2).is_ok());

        let result = router.get_or_load(&repo3);
        assert!(result.is_err());
        // Both original repos still loaded (no eviction)
        assert_eq!(router.repo_count(), 2);
    }

    #[test]
    fn expiry_blocks_new_requests() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        make_test_repo(&repo);

        let router = RepoRouter::new(Tier::new(
            "Pro",
            TierLimits {
                max_repos: 10,
                ..TierLimits::PRO
            },
        ));
        // Set expiry to the past
        router.set_expires_at(1);

        let result = router.get_or_load(&repo);
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("expired"), "unexpected error: {err_msg}");
    }

    #[test]
    fn sweep_expired_frees_repos() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        make_test_repo(&repo);

        let router = RepoRouter::new(Tier::new(
            "Pro",
            TierLimits {
                max_repos: 10,
                ..TierLimits::PRO
            },
        ));
        router.get_or_load(&repo).unwrap();
        assert_eq!(router.repo_count(), 1);

        // Not expired yet — sweep does nothing
        assert_eq!(router.sweep_expired(), 0);
        assert_eq!(router.repo_count(), 1);

        // Expire the subscription
        router.set_expires_at(1);
        let freed = router.sweep_expired();
        assert_eq!(freed, 1);
        assert_eq!(router.repo_count(), 0);
    }

    #[test]
    fn free_tier_no_expiry() {
        let router = RepoRouter::new(Tier::new("Free", TierLimits::FREE));
        // Free tier: expires_at stays 0 (no expiry)
        assert!(!router.is_expired());
        assert_eq!(router.sweep_expired(), 0);
    }

    #[test]
    fn dedup_prevents_double_load() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        make_test_repo(&repo);

        let router = Arc::new(RepoRouter::new(Tier::new(
            "Pro",
            TierLimits {
                max_repos: 10,
                ..TierLimits::PRO
            },
        )));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let router = Arc::clone(&router);
                let repo = repo.clone();
                std::thread::spawn(move || router.get_or_load(&repo).is_ok())
            })
            .collect();

        for h in handles {
            assert!(h.join().unwrap());
        }
        assert_eq!(router.repo_count(), 1);
    }

    #[test]
    fn tier_parse_roundtrip() {
        // Bare tier names → defaults
        let free: Tier = "free".parse().unwrap();
        assert_eq!(free.max_repos(), 1);
        assert_eq!(free.max_files(), 250);
        assert_eq!(free.max_loc(), 10_000);

        let pro: Tier = "pro".parse().unwrap();
        assert_eq!(pro.max_repos(), 10);
        assert_eq!(pro.max_files(), 5_000);
        assert_eq!(pro.max_loc(), 200_000);

        // Repos only
        let free3: Tier = "free:3".parse().unwrap();
        assert_eq!(free3.max_repos(), 3);
        assert_eq!(free3.max_files(), 250); // default preserved

        // Repos + files
        let pro_custom: Tier = "pro:100:50000".parse().unwrap();
        assert_eq!(pro_custom.max_repos(), 100);
        assert_eq!(pro_custom.max_files(), 50_000);
        assert_eq!(pro_custom.max_loc(), 200_000); // default preserved

        // All three
        let full: Tier = "free:5:10000:1000000".parse().unwrap();
        assert_eq!(full.max_repos(), 5);
        assert_eq!(full.max_files(), 10_000);
        assert_eq!(full.max_loc(), 1_000_000);

        // Errors
        assert!("unknown".parse::<Tier>().is_err());
        assert!("free:abc".parse::<Tier>().is_err());
    }

    #[test]
    fn file_limit_rejects_large_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("big_repo");
        std::fs::create_dir_all(&repo).unwrap();

        // Create 10 source files
        for i in 0..10 {
            std::fs::write(
                repo.join(format!("mod_{i}.rs")),
                format!("fn func_{i}() {{}}\n"),
            )
            .unwrap();
        }
        // Git init
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .output()
            .ok();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&repo)
            .output()
            .ok();
        std::process::Command::new("git")
            .args(["commit", "-m", "init", "--quiet"])
            .current_dir(&repo)
            .output()
            .ok();

        // Tier with max 5 files — should reject
        let tiny_tier = Tier::new(
            "Free",
            TierLimits {
                max_repos: 1,
                max_files: 5,
                max_loc: 1_000_000,
            },
        );
        let router = RepoRouter::new(tiny_tier);
        let result = router.get_or_load(&repo);
        assert!(result.is_err());
        let err = result.err().unwrap().to_string();
        assert!(err.contains("source files"), "unexpected error: {err}");
        assert!(err.contains("Upgrade"), "should suggest upgrade: {err}");
    }

    #[test]
    fn file_limit_ignores_configured_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("ignored_repo");
        std::fs::create_dir_all(repo.join("ignored_src/pkg")).unwrap();

        std::fs::write(repo.join("main.rs"), "fn main() {}\n").unwrap();
        for i in 0..10 {
            std::fs::write(
                repo.join(format!("ignored_src/pkg/file_{i}.rs")),
                format!("pub fn ignored_{i}() {{}}\n"),
            )
            .unwrap();
        }

        let cfg = Config {
            ignore_patterns: vec!["ignored_src".into()],
            ..Default::default()
        };
        cfg.save(&repo).unwrap();

        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .output()
            .ok();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&repo)
            .output()
            .ok();
        std::process::Command::new("git")
            .args(["commit", "-m", "init", "--quiet"])
            .current_dir(&repo)
            .output()
            .ok();

        let tiny_tier = Tier::new(
            "Free",
            TierLimits {
                max_repos: 1,
                max_files: 5,
                max_loc: 1_000_000,
            },
        );
        let router = RepoRouter::new(tiny_tier);
        let result = router.get_or_load(&repo);
        assert!(
            result.is_ok(),
            "ignored files should not count against file gate"
        );
    }

    #[test]
    fn loc_limit_rejects_large_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("loc_repo");
        std::fs::create_dir_all(&repo).unwrap();

        // Create 3 files with ~100 lines each = ~300 LOC
        for i in 0..3 {
            let content: String = (0..100)
                .map(|j| format!("fn func_{i}_{j}() {{ todo!() }}\n"))
                .collect();
            std::fs::write(repo.join(format!("mod_{i}.rs")), content).unwrap();
        }
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .output()
            .ok();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&repo)
            .output()
            .ok();
        std::process::Command::new("git")
            .args(["commit", "-m", "init", "--quiet"])
            .current_dir(&repo)
            .output()
            .ok();

        // Tier with max 100 LOC — should reject (~300 LOC repo)
        let tiny_tier = Tier::new(
            "Free",
            TierLimits {
                max_repos: 1,
                max_files: 10_000,
                max_loc: 100,
            },
        );
        let router = RepoRouter::new(tiny_tier);
        let result = router.get_or_load(&repo);
        assert!(result.is_err());
        let err = result.err().unwrap().to_string();
        assert!(err.contains("lines of code"), "unexpected error: {err}");
        assert!(err.contains("Upgrade"), "should suggest upgrade: {err}");
    }

    /// Regression: router-loaded repos must run `init_embed` so semantic
    /// search works for any repo activated through `code_activate_repo`,
    /// not just the primary `--repo`. Pre-fix, `load_repo` skipped the
    /// embed init and `embed_service` stayed `None` → semantic mode
    /// failed closed even with valid credentials.
    #[test]
    fn router_load_initializes_embed_service_when_credentials_present() {
        // Env vars are process-global; serialize access.
        static ROUTER_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ROUTER_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        make_test_repo(&repo);

        let cred_dir = dir.path().join("creds");
        std::fs::create_dir_all(&cred_dir).unwrap();
        std::fs::write(
            cred_dir.join("credentials.json"),
            r#"{"key":"sk-recon-test-router-embed"}"#,
        )
        .unwrap();

        unsafe {
            std::env::set_var("RECON_CONFIG_DIR", &cred_dir);
            std::env::remove_var("RECON_NO_EMBED");
        }

        let router = RepoRouter::new(Tier::new(
            "Pro",
            TierLimits {
                max_repos: 10,
                ..TierLimits::PRO
            },
        ));
        let server = router.get_or_load(&repo).expect("repo loads");

        unsafe {
            std::env::remove_var("RECON_CONFIG_DIR");
        }

        assert!(
            server.embed_service.read().is_some(),
            "embed_service must be Some after router load when creds are present"
        );
    }
}
