//! Multi-repo hosted server with API key auth and lazy repo loading.

use anyhow::Result;
use dashmap::DashMap;
use recon_indexer::indexer;
use recon_search::tantivy_backend::TantivyBackend;
use recon_server::server::ReconServer;
use recon_storage::store::Store;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

/// API key → repo path mapping.
#[derive(Debug, Clone)]
pub struct KeyConfig {
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

/// Lazily-loaded repo state: SQLite + Tantivy + ReconServer.
struct RepoState {
    server: ReconServer,
}

/// Multi-repo manager backed by DashMap for concurrent access.
pub struct RepoRouter {
    repos: DashMap<PathBuf, Arc<RepoState>>,
}

impl RepoRouter {
    pub fn new() -> Self {
        Self {
            repos: DashMap::new(),
        }
    }

    /// Get or lazily load a ReconServer for the given repo path.
    pub fn get_or_load(&self, repo_path: &Path) -> Result<ReconServer> {
        // Fast path: already loaded
        if let Some(state) = self.repos.get(repo_path) {
            return Ok(state.server.clone());
        }

        // Slow path: load and index
        let repo_path = repo_path.canonicalize()?;
        let store_dir = repo_path.join(".recon");
        std::fs::create_dir_all(&store_dir)?;

        let store = Store::open(&store_dir.join("index.db")).map_err(|e| anyhow::anyhow!("{e}"))?;
        let tantivy =
            TantivyBackend::open(&store_dir.join("tantivy")).map_err(|e| anyhow::anyhow!("{e}"))?;

        // Incremental index on first load
        match indexer::index_repo_incremental(&store, Some(&tantivy), &repo_path) {
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

        let server = ReconServer::new(repo_path.clone(), store, tantivy);
        let state = Arc::new(RepoState {
            server: server.clone(),
        });
        self.repos.insert(repo_path, state);
        Ok(server)
    }

    /// Number of loaded repos.
    pub fn repo_count(&self) -> usize {
        self.repos.len()
    }
}

/// Extract Bearer token from Authorization header value.
pub fn extract_bearer(header_value: &str) -> Option<&str> {
    header_value
        .strip_prefix("Bearer ")
        .or_else(|| header_value.strip_prefix("bearer "))
}
