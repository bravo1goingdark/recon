//! Configuration file support for recon.
//!
//! Reads `.recon/config.toml` from the repo root. Falls back to defaults
//! if the file doesn't exist.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Recon configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Additional path patterns to ignore (on top of .gitignore).
    pub ignore_patterns: Vec<String>,

    /// Maximum file size to index in bytes (default 1MB).
    pub max_file_size: u64,

    /// Maximum file size for embedding pipeline in bytes (default 100KB).
    pub max_embed_size: u64,

    /// File watcher debounce interval in milliseconds (default 250).
    pub watcher_debounce_ms: u64,

    /// Tantivy writer heap size in bytes (default 50MB).
    pub tantivy_heap_bytes: usize,

    /// Maximum results per search tool call (default 30).
    pub max_search_results: usize,

    /// Default token budget for code_repo_map (default 2000).
    pub default_map_budget: usize,

    /// Enable secret redaction on responses (default true).
    pub redact_secrets: bool,

    /// Allow access to sensitive files (.env, .pem, etc) (default false).
    pub allow_sensitive: bool,

    /// Edge weight configuration for PPR graph construction.
    /// When absent, Aider-style defaults are used.
    pub edge_weights: Option<EdgeWeights>,
}

/// Edge weight configuration for PPR graph construction.
/// All values follow Aider-style multiplicative heuristics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EdgeWeights {
    /// Multiplier for descriptive identifiers (>8 chars, contains _ or uppercase).
    /// Default: 10.0
    pub descriptive_ident_mult: f64,
    /// Multiplier for underscore-prefixed identifiers (private/internal convention).
    /// Default: 0.1
    pub private_ident_mult: f64,
    /// Multiplier for high-fan-out identifiers (resolves to >5 symbols).
    /// Default: 0.1
    pub high_fanout_mult: f64,
    /// Threshold for high-fan-out classification.
    /// Default: 5
    pub high_fanout_threshold: usize,
    /// Multiplier for references from focus-set symbols.
    /// Default: 50.0
    pub focus_boost: f64,
}

impl Default for EdgeWeights {
    fn default() -> Self {
        Self {
            descriptive_ident_mult: 10.0,
            private_ident_mult: 0.1,
            high_fanout_mult: 0.1,
            high_fanout_threshold: 5,
            focus_boost: 50.0,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ignore_patterns: Vec::new(),
            max_file_size: 1_048_576, // 1 MB
            max_embed_size: 102_400,  // 100 KB
            watcher_debounce_ms: 250,
            tantivy_heap_bytes: 50_000_000, // 50 MB
            max_search_results: 30,
            default_map_budget: 2000,
            redact_secrets: true,
            allow_sensitive: false,
            edge_weights: None,
        }
    }
}

impl Config {
    /// Load config from `.recon/config.toml` in the repo root.
    /// Returns default config if the file doesn't exist.
    pub fn load(repo_root: &Path) -> Self {
        let config_path = repo_root.join(".recon").join("config.toml");
        match std::fs::read_to_string(&config_path) {
            Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
                tracing::warn!("invalid config.toml, using defaults: {e}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Write the current config to `.recon/config.toml`.
    pub fn save(&self, repo_root: &Path) -> std::io::Result<()> {
        let config_dir = repo_root.join(".recon");
        std::fs::create_dir_all(&config_dir)?;
        let content = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(config_dir.join("config.toml"), content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.max_file_size, 1_048_576);
        assert!(cfg.redact_secrets);
        assert!(!cfg.allow_sensitive);
    }

    #[test]
    fn load_missing_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path());
        assert_eq!(cfg.max_file_size, 1_048_576);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            ignore_patterns: vec!["*.generated.*".into(), "dist/".into()],
            max_search_results: 50,
            ..Default::default()
        };
        cfg.save(dir.path()).unwrap();

        let loaded = Config::load(dir.path());
        assert_eq!(loaded.ignore_patterns.len(), 2);
        assert_eq!(loaded.max_search_results, 50);
    }

    #[test]
    fn load_partial_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join(".recon");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            "max_file_size = 500000\nredact_secrets = false\n",
        )
        .unwrap();

        let cfg = Config::load(dir.path());
        assert_eq!(cfg.max_file_size, 500_000);
        assert!(!cfg.redact_secrets);
        // Other fields should be defaults
        assert_eq!(cfg.max_search_results, 30);
    }
}
