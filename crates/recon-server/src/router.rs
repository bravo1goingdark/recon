//! Multi-repo router with DashMap for lock-free concurrent access.
//!
//! Supports Free and Pro tiers with configurable repo limits.
//! Pro tier supports subscription expiry — repos are served until expiry,
//! then cleaned up to free memory. No surprise LRU eviction for paying users.

use crate::server::ReconServer;
use dashmap::DashMap;
use recon_indexer::indexer;
use recon_search::tantivy_backend::TantivyBackend;
use recon_storage::store::Store;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

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

/// Default limits per tier.
impl TierLimits {
    /// Free tier defaults: 1 repo, 2K files, 200K LOC.
    pub const FREE: Self = Self {
        max_repos: 1,
        max_files: 2_000,
        max_loc: 200_000,
    };

    /// Pro tier defaults: 50 repos, 20K files, 2M LOC.
    pub const PRO: Self = Self {
        max_repos: 50,
        max_files: 20_000,
        max_loc: 2_000_000,
    };

    /// Uncapped (self-hosted / enterprise).
    pub const UNCAPPED: Self = Self {
        max_repos: usize::MAX,
        max_files: usize::MAX,
        max_loc: usize::MAX,
    };
}

/// Subscription tier controlling repo and indexing limits.
///
/// Both tiers carry configurable limits. The key difference:
/// - **Free**: no expiry, repos live forever, rejects at limit.
/// - **Pro**: has an optional expiry. Repos live until expiry, then
///   `sweep_expired()` cleans them up to reclaim memory. Rejects at limit
///   (no LRU eviction — paying users keep their repos).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Free tier.
    Free {
        /// Resource limits.
        limits: TierLimits,
    },
    /// Pro tier.
    Pro {
        /// Resource limits.
        limits: TierLimits,
    },
}

impl Tier {
    /// Get the resource limits for this tier.
    pub fn limits(self) -> TierLimits {
        match self {
            Tier::Free { limits } | Tier::Pro { limits } => limits,
        }
    }

    /// Maximum number of repos allowed.
    pub fn max_repos(self) -> usize {
        self.limits().max_repos
    }

    /// Maximum source files per repo.
    pub fn max_files(self) -> usize {
        self.limits().max_files
    }

    /// Maximum lines of code per repo.
    pub fn max_loc(self) -> usize {
        self.limits().max_loc
    }

    /// Human-readable tier name for error messages.
    pub fn name(self) -> &'static str {
        match self {
            Tier::Free { .. } => "Free",
            Tier::Pro { .. } => "Pro",
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let l = self.limits();
        write!(
            f,
            "{} (max {} repos, {} files, {}K LOC)",
            self.name(),
            l.max_repos,
            l.max_files,
            l.max_loc / 1000,
        )
    }
}

impl std::str::FromStr for Tier {
    type Err = String;

    /// Parse tier from CLI: `free`, `free:N`, `pro`, or `pro:N` (N = max repos).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.to_lowercase();
        if lower == "free" {
            return Ok(Tier::Free {
                limits: TierLimits::FREE,
            });
        }
        if lower == "pro" {
            return Ok(Tier::Pro {
                limits: TierLimits::PRO,
            });
        }
        if let Some(n_str) = lower.strip_prefix("free:") {
            let n = n_str
                .parse::<usize>()
                .map_err(|e| format!("invalid max_repos: {e}"))?;
            return Ok(Tier::Free {
                limits: TierLimits {
                    max_repos: n,
                    ..TierLimits::FREE
                },
            });
        }
        if let Some(n_str) = lower.strip_prefix("pro:") {
            let n = n_str
                .parse::<usize>()
                .map_err(|e| format!("invalid max_repos: {e}"))?;
            return Ok(Tier::Pro {
                limits: TierLimits {
                    max_repos: n,
                    ..TierLimits::PRO
                },
            });
        }
        Err(format!(
            "unknown tier '{s}': expected 'free', 'free:N', 'pro', or 'pro:N'"
        ))
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
    pub fn get_or_load(&self, repo_path: &Path) -> Result<ReconServer, anyhow::Error> {
        if self.is_expired() {
            return Err(anyhow::anyhow!(
                "subscription expired — renew to continue using Pro features"
            ));
        }

        let repo_path = repo_path
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("canonicalize: {e}"))?;

        // Check tier limit before loading
        if !self.repos.contains_key(&repo_path) && self.repos.len() >= self.tier.max_repos() {
            return Err(anyhow::anyhow!(
                "repo limit reached ({} max for {} tier)",
                self.tier.max_repos(),
                self.tier,
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
    fn load_repo(repo_path: &Path, tier: Tier) -> Result<ReconServer, anyhow::Error> {
        let limits = tier.limits();
        let tier_name = tier.name();

        // Pre-flight: walk the repo and check file count before any indexing
        let paths = recon_indexer::walker::walk_repo(repo_path);
        let file_count = paths.len();

        if file_count > limits.max_files {
            return Err(anyhow::anyhow!(
                "Repository has {} source files — exceeds the {} tier limit of {} files. \
                 Upgrade to a higher tier for larger repositories.",
                file_count,
                tier_name,
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
                return Err(anyhow::anyhow!(
                    "Repository has approximately {}K lines of code — exceeds the {} tier \
                     limit of {}K LOC. Upgrade to a higher tier for larger repositories.",
                    estimated_loc / 1000,
                    tier_name,
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
        std::fs::create_dir_all(&store_dir)?;

        let store = Store::open(&store_dir.join("index.db")).map_err(|e| anyhow::anyhow!("{e}"))?;
        let tantivy =
            TantivyBackend::open(&store_dir.join("tantivy")).map_err(|e| anyhow::anyhow!("{e}"))?;

        let mut writer = tantivy.writer(50_000_000).ok();
        match indexer::index_repo_incremental(&store, Some(&tantivy), repo_path, writer.as_mut()) {
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

        Ok(ReconServer::new(repo_path.to_path_buf(), store, tantivy))
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

        let router = RepoRouter::new(Tier::Free {
            limits: TierLimits::FREE,
        });
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
        let router = RepoRouter::new(Tier::Free {
            limits: TierLimits {
                max_repos: 2,
                ..TierLimits::FREE
            },
        });
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

        let router = RepoRouter::new(Tier::Pro {
            limits: TierLimits {
                max_repos: 10,
                ..TierLimits::PRO
            },
        });
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
        let router = RepoRouter::new(Tier::Pro {
            limits: TierLimits {
                max_repos: 2,
                ..TierLimits::PRO
            },
        });
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

        let router = RepoRouter::new(Tier::Pro {
            limits: TierLimits {
                max_repos: 10,
                ..TierLimits::PRO
            },
        });
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

        let router = RepoRouter::new(Tier::Pro {
            limits: TierLimits {
                max_repos: 10,
                ..TierLimits::PRO
            },
        });
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
        let router = RepoRouter::new(Tier::Free {
            limits: TierLimits::FREE,
        });
        // Free tier: expires_at stays 0 (no expiry)
        assert!(!router.is_expired());
        assert_eq!(router.sweep_expired(), 0);
    }

    #[test]
    fn dedup_prevents_double_load() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        make_test_repo(&repo);

        let router = Arc::new(RepoRouter::new(Tier::Pro {
            limits: TierLimits {
                max_repos: 10,
                ..TierLimits::PRO
            },
        }));

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
        let free: Tier = "free".parse().unwrap();
        assert_eq!(free.max_repos(), 1);

        let free3: Tier = "free:3".parse().unwrap();
        assert_eq!(free3.max_repos(), 3);

        let pro: Tier = "pro".parse().unwrap();
        assert_eq!(pro.max_repos(), 50);

        let pro100: Tier = "pro:100".parse().unwrap();
        assert_eq!(pro100.max_repos(), 100);

        assert!("unknown".parse::<Tier>().is_err());
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
        let tiny_tier = Tier::Free {
            limits: TierLimits {
                max_repos: 1,
                max_files: 5,
                max_loc: 1_000_000,
            },
        };
        let router = RepoRouter::new(tiny_tier);
        let result = router.get_or_load(&repo);
        assert!(result.is_err());
        let err = result.err().unwrap().to_string();
        assert!(err.contains("source files"), "unexpected error: {err}");
        assert!(err.contains("Upgrade"), "should suggest upgrade: {err}");
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
        let tiny_tier = Tier::Free {
            limits: TierLimits {
                max_repos: 1,
                max_files: 10_000,
                max_loc: 100,
            },
        };
        let router = RepoRouter::new(tiny_tier);
        let result = router.get_or_load(&repo);
        assert!(result.is_err());
        let err = result.err().unwrap().to_string();
        assert!(err.contains("lines of code"), "unexpected error: {err}");
        assert!(err.contains("Upgrade"), "should suggest upgrade: {err}");
    }
}
