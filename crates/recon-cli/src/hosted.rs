//! Multi-repo hosted server with API key auth and lazy repo loading.

use anyhow::Result;
use recon_server::router::{RepoRouter, Tier, TierLimits};
use recon_server::server::ReconServer;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::info;

/// API key → repo path mapping.
#[derive(Debug, Clone)]
pub struct KeyConfig {
    /// Map from API key to repo path.
    pub keys: HashMap<String, PathBuf>,
}

impl KeyConfig {
    /// Load from a JSON file:
    /// ```json
    /// {
    ///   "keys": {
    ///     "sk-abc123": "/srv/repos/my-project",
    ///     "sk-def456": "/srv/repos/other-project"
    ///   }
    /// }
    /// ```
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let val: serde_json::Value = serde_json::from_str(&content)?;
        let keys_obj = val
            .get("keys")
            .and_then(|v| v.as_object())
            .ok_or_else(|| anyhow::anyhow!("missing \"keys\" object in config"))?;

        let mut keys = HashMap::new();
        for (key, val) in keys_obj {
            let path = val
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("key {key}: value must be a path string"))?;
            keys.insert(key.clone(), PathBuf::from(path));
        }

        info!(count = keys.len(), "loaded API keys");
        Ok(Self { keys })
    }

    /// Look up the repo path for an API key.
    pub fn repo_for_key(&self, key: &str) -> Option<&Path> {
        self.keys.get(key).map(|p| p.as_path())
    }
}

/// Create a `RepoRouter` configured for the hosted environment.
///
/// Uses Pro tier with a configurable max repo count.
#[allow(dead_code)]
pub fn hosted_router(max_repos: usize) -> RepoRouter {
    RepoRouter::new(Tier::new(
        "Pro",
        TierLimits {
            max_repos,
            ..TierLimits::PRO
        },
    ))
}

/// Resolve an API key to a `ReconServer`, loading the repo if needed.
#[allow(dead_code)]
pub fn server_for_key(
    router: &RepoRouter,
    config: &KeyConfig,
    api_key: &str,
) -> Result<ReconServer> {
    let repo_path = config
        .repo_for_key(api_key)
        .ok_or_else(|| anyhow::anyhow!("unknown API key"))?;
    router.get_or_load(repo_path)
}

/// Extract Bearer token from Authorization header value.
pub fn extract_bearer(header_value: &str) -> Option<&str> {
    header_value
        .strip_prefix("Bearer ")
        .or_else(|| header_value.strip_prefix("bearer "))
}
