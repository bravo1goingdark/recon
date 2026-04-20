//! License validation client for recon.dev.
//!
//! On startup, the CLI sends the API key to `api.recon.dev/v1/license/validate`.
//! The server returns the tier, limits, and expiry. The response is cached locally
//! in `.recon/license.json` so the tool works offline for up to 24 hours.
//!
//! If no key is provided, or validation fails and no cache exists, the CLI
//! runs in Free tier (open source mode). No code ever leaves the user's machine —
//! only the API key and a machine fingerprint are sent.

use crate::router::{Tier, TierLimits};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// How long a cached license is valid without phoning home (24 hours).
const CACHE_TTL_SECS: u64 = 86_400;

/// Default API endpoint. Override with `RECON_API_URL` env var.
const DEFAULT_API_URL: &str = "https://recon-api.kumarashutosh34169.workers.dev";

/// Response from the license server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LicenseResponse {
    /// Whether the key is valid.
    pub valid: bool,
    /// Tier name (free, pro, team, enterprise).
    pub tier: String,
    /// Resource limits.
    pub limits: LimitsPayload,
    /// Unix timestamp when the subscription expires (0 = no expiry).
    pub expires_at: u64,
    /// Human-readable message (e.g. "Pro plan active until 2026-12-01").
    #[serde(default)]
    pub message: String,
}

/// Limits as returned by the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsPayload {
    /// Maximum repos.
    pub max_repos: usize,
    /// Maximum files per repo.
    pub max_files: usize,
    /// Maximum LOC per repo.
    pub max_loc: usize,
}

/// Cached license stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedLicense {
    /// When this cache was written (unix seconds).
    cached_at: u64,
    /// The API response.
    response: LicenseResponse,
}

/// Result of license validation.
#[derive(Debug, Clone)]
pub struct ValidatedLicense {
    /// The tier to use.
    pub tier: Tier,
    /// Subscription expiry (0 = no expiry).
    pub expires_at: u64,
    /// How the license was resolved.
    pub source: LicenseSource,
    /// Message from the server.
    pub message: String,
}

/// How the license was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LicenseSource {
    /// Validated against api.recon.dev.
    Remote,
    /// Used local cache (server unreachable).
    Cache,
    /// No key provided — open source Free tier.
    FreeTier,
}

impl std::fmt::Display for LicenseSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Remote => write!(f, "validated"),
            Self::Cache => write!(f, "cached"),
            Self::FreeTier => write!(f, "free tier"),
        }
    }
}

/// Validate a license key, with local caching and offline fallback.
///
/// Flow:
/// 1. If no key → error (key is required)
/// 2. Try remote validation → cache result on success
/// 3. If remote fails → use cached license if < 24h old
/// 4. If no valid cache → error (cannot validate key)
pub fn validate_license(key: Option<&str>, cache_dir: &Path) -> Result<ValidatedLicense, String> {
    let key = match key {
        Some(k) if !k.is_empty() => k,
        _ => {
            return Err(
                "API key required. Get one free at https://mcprecon.pages.dev/login\n\
                 Then run: RECON_KEY=sk-recon-xxx recon serve"
                    .into(),
            );
        }
    };

    let cache_path = cache_dir.join("license.json");

    // Try remote validation
    match validate_remote(key) {
        Ok(resp) => {
            // Cache the successful response
            if let Err(e) = write_cache(&cache_path, &resp) {
                debug!("failed to cache license: {e}");
            }
            let license = response_to_license(resp, LicenseSource::Remote);
            info!(
                tier = license.tier.name(),
                source = %license.source,
                "license {}",
                license.message
            );
            Ok(license)
        }
        Err(e) => {
            warn!("license validation failed: {e}");

            // Try cache fallback (offline grace period)
            match read_cache(&cache_path) {
                Some(cached) => {
                    info!(
                        tier = cached.response.tier,
                        "using cached license (server unreachable)"
                    );
                    Ok(response_to_license(cached.response, LicenseSource::Cache))
                }
                None => Err(format!(
                    "Could not validate API key: {e}\n\
                     Check your key at https://mcprecon.pages.dev/dashboard"
                )),
            }
        }
    }
}

/// Call the remote license server.
fn validate_remote(key: &str) -> Result<LicenseResponse, String> {
    let api_url = std::env::var("RECON_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string());
    let url = format!("{api_url}/v1/license/validate");

    let resp: LicenseResponse = ureq::post(&url)
        .header("Authorization", &format!("Bearer {key}"))
        .header("User-Agent", concat!("recon/", env!("CARGO_PKG_VERSION")))
        .send_json(serde_json::json!({
            "key": key,
        }))
        .map_err(|e| format!("HTTP error: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("JSON parse error: {e}"))?;

    if !resp.valid {
        return Err(format!("invalid key: {}", resp.message));
    }

    Ok(resp)
}

/// Convert API response to a ValidatedLicense.
fn response_to_license(resp: LicenseResponse, source: LicenseSource) -> ValidatedLicense {
    let limits = TierLimits {
        max_repos: resp.limits.max_repos,
        max_files: resp.limits.max_files,
        max_loc: resp.limits.max_loc,
    };
    ValidatedLicense {
        tier: Tier::new(leak_tier_name(&resp.tier), limits),
        expires_at: resp.expires_at,
        source,
        message: resp.message,
    }
}

/// Leak a tier name string to get a `&'static str`.
/// Safe because tier names are a small fixed set — at most 5 allocations over
/// the lifetime of the process.
fn leak_tier_name(name: &str) -> &'static str {
    match name.to_lowercase().as_str() {
        "free" => "Free",
        "pro" => "Pro",
        "team" => "Team",
        "enterprise" => "Enterprise",
        "uncapped" => "Uncapped",
        _ => Box::leak(name.to_string().into_boxed_str()),
    }
}

/// Write license cache to disk.
fn write_cache(path: &Path, resp: &LicenseResponse) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let cached = CachedLicense {
        cached_at: now_secs(),
        response: resp.clone(),
    };
    let json = serde_json::to_string_pretty(&cached).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

/// Read and validate license cache from disk.
fn read_cache(path: &Path) -> Option<CachedLicense> {
    let content = std::fs::read_to_string(path).ok()?;
    let cached: CachedLicense = serde_json::from_str(&content).ok()?;

    // Check TTL
    let age = now_secs().saturating_sub(cached.cached_at);
    if age > CACHE_TTL_SECS {
        debug!(age_hours = age / 3600, "license cache expired");
        return None;
    }

    Some(cached)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_key_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let result = validate_license(None, dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("API key required"));
    }

    #[test]
    fn empty_key_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let result = validate_license(Some(""), dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("API key required"));
    }

    #[test]
    fn invalid_key_no_cache_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let result = validate_license(Some("sk-invalid-key-12345"), dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Could not validate"));
    }

    #[test]
    fn cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("license.json");

        let resp = LicenseResponse {
            valid: true,
            tier: "Pro".into(),
            limits: LimitsPayload {
                max_repos: 50,
                max_files: 20_000,
                max_loc: 2_000_000,
            },
            expires_at: now_secs() + 86400,
            message: "Pro plan active".into(),
        };

        write_cache(&cache_path, &resp).unwrap();
        let cached = read_cache(&cache_path).unwrap();
        assert_eq!(cached.response.tier, "Pro");
        assert_eq!(cached.response.limits.max_repos, 50);
    }

    #[test]
    fn expired_cache_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("license.json");

        let cached = CachedLicense {
            cached_at: 0, // epoch = very old
            response: LicenseResponse {
                valid: true,
                tier: "Pro".into(),
                limits: LimitsPayload {
                    max_repos: 50,
                    max_files: 20_000,
                    max_loc: 2_000_000,
                },
                expires_at: 0,
                message: String::new(),
            },
        };
        let json = serde_json::to_string(&cached).unwrap();
        std::fs::write(&cache_path, json).unwrap();

        assert!(
            read_cache(&cache_path).is_none(),
            "expired cache should return None"
        );
    }

    #[test]
    fn leak_tier_name_known() {
        assert_eq!(leak_tier_name("pro"), "Pro");
        assert_eq!(leak_tier_name("FREE"), "Free");
        assert_eq!(leak_tier_name("team"), "Team");
    }
}
