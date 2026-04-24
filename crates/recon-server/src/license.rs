//! License validation client for recon.dev.
//!
//! Caching model:
//! - `recon login <key>` validates remotely and writes `~/.config/recon/license.json`.
//! - All other commands call `validate_license(None, &global_config_dir())` — reads
//!   the cache only, no network. Works offline for up to 24 hours.
//! - No source code leaves the machine — only the API key and CLI version are sent.
//!
//! ## Tamper protection
//! Every cached response is HMAC-SHA256 signed by the server over the canonical
//! payload `"{tier}:{max_repos}:{max_files}:{max_loc}:{expires_at}"`.  The CLI
//! verifies the signature on every cache read and on every remote response.
//! An absent or invalid signature is always rejected, preventing a user from
//! editing `~/.config/recon/license.json` to upgrade their own tier.
//!
//! The HMAC key is embedded at compile time via `RECON_LICENSE_HMAC_KEY` (falls
//! back to a dev placeholder when the env var is not set).  The production key
//! must match the `LICENSE_HMAC_SECRET` Cloudflare Worker secret.

use crate::router::{Tier, TierLimits};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

type HmacSha256 = Hmac<Sha256>;

/// Create an HMAC-SHA256 instance from the compile-time key.
///
/// Returns `None` only if the key is empty, which cannot happen with the
/// current `HMAC_KEY` definition (always at least the dev placeholder).
fn new_mac() -> Option<HmacSha256> {
    HmacSha256::new_from_slice(HMAC_KEY).ok()
}

/// How long a cached license is valid without phoning home (24 hours).
const CACHE_TTL_SECS: u64 = 86_400;

/// Default API endpoint. Override with `RECON_API_URL` env var.
const DEFAULT_API_URL: &str = "https://recon-api.kumarashutosh34169.workers.dev";

/// HMAC-SHA256 key embedded at compile time.
///
/// Set `RECON_LICENSE_HMAC_KEY` in the build environment to the production key.
/// Without it the binary uses a dev placeholder and will only trust dev-signed
/// responses (which is fine for local development and test suites).
const HMAC_KEY: &[u8] = if let Some(k) = option_env!("RECON_LICENSE_HMAC_KEY") {
    k.as_bytes()
} else {
    b"recon-dev-hmac-key-not-for-production-00000000"
};

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
    /// HMAC-SHA256 signature over `"{tier}:{max_repos}:{max_files}:{max_loc}:{expires_at}"`.
    /// `None` means the response predates signing; treated as invalid by strict checks.
    #[serde(default)]
    pub signature: Option<String>,
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

/// Result of a successful license validation.
#[derive(Debug, Clone)]
pub struct ValidatedLicense {
    /// The tier to enforce.
    pub tier: Tier,
    /// Subscription expiry (0 = no expiry).
    pub expires_at: u64,
    /// How the license was resolved.
    pub source: LicenseSource,
    /// Human-readable message from the server.
    pub message: String,
    /// Set to `true` when the periodic re-validation task has observed a
    /// hard rejection from the worker (revoked key, deleted key). Distinct
    /// from natural expiry: the gate treats both as "expired" for the
    /// agent-facing error message, but telemetry/diagnostics can tell them
    /// apart. Not persisted in the signed cache — it's runtime-only state.
    pub revoked: bool,
}

impl ValidatedLicense {
    /// Whether the license has ceased to be valid — either naturally via
    /// `expires_at` or via an explicit `revoked` flag set by the
    /// re-validation task when the worker returns 401.
    ///
    /// `expires_at == 0` means "no expiry" (Free tier fallback, or a legacy
    /// perpetual one-time-purchase license) — on its own, returns false,
    /// but `revoked = true` still fires the gate.
    pub fn is_expired(&self) -> bool {
        if self.revoked {
            return true;
        }
        self.expires_at > 0 && self.expires_at < now_secs()
    }

    /// Format the expiry as an ISO-8601 UTC date for user-facing messages.
    ///
    /// Returns "never" for `expires_at == 0` so the caller can render a
    /// consistent string without an extra branch.
    pub fn expiry_string(&self) -> String {
        if self.expires_at == 0 {
            return "never".to_string();
        }
        // Render YYYY-MM-DD HH:MM UTC — cheap, avoids pulling in chrono.
        let secs = self.expires_at as i64;
        let days_since_epoch = secs / 86_400;
        let time_of_day = secs % 86_400;
        let (year, month, day) = days_to_ymd(days_since_epoch);
        let hh = time_of_day / 3600;
        let mm = (time_of_day % 3600) / 60;
        format!("{year:04}-{month:02}-{day:02} {hh:02}:{mm:02} UTC")
    }
}

/// Convert days-since-Unix-epoch to a (year, month, day) triple.
///
/// Algorithm from Howard Hinnant's "date" library (civil_from_days), adapted
/// for i64. Tiny, allocation-free, no dependency.
fn days_to_ymd(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

impl ValidatedLicense {
    /// Whether this license has passed its billing-period end.
    ///
    /// `expires_at == 0` means "no expiry" (Free tier fallback, or a legacy
    /// perpetual one-time-purchase license) — always returns false.
    pub fn is_expired(&self) -> bool {
        self.expires_at > 0 && self.expires_at < now_secs()
    }

    /// Format the expiry as an ISO-8601 UTC date for user-facing messages.
    ///
    /// Returns "never" for `expires_at == 0` so the caller can render a
    /// consistent string without an extra branch.
    pub fn expiry_string(&self) -> String {
        if self.expires_at == 0 {
            return "never".to_string();
        }
        // Render YYYY-MM-DD HH:MM UTC — cheap, avoids pulling in chrono.
        let secs = self.expires_at as i64;
        let days_since_epoch = secs / 86_400;
        let time_of_day = secs % 86_400;
        let (year, month, day) = days_to_ymd(days_since_epoch);
        let hh = time_of_day / 3600;
        let mm = (time_of_day % 3600) / 60;
        format!("{year:04}-{month:02}-{day:02} {hh:02}:{mm:02} UTC")
    }
}

/// Convert days-since-Unix-epoch to a (year, month, day) triple.
///
/// Algorithm from Howard Hinnant's "date" library (civil_from_days), adapted
/// for i64. Tiny, allocation-free, no dependency.
fn days_to_ymd(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// How the license was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LicenseSource {
    /// Validated live against api.recon.dev.
    Remote,
    /// Read from the local cache (server unreachable, or no key supplied).
    Cache,
    /// No key and no cache — Free tier fallback.
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

/// Returns the platform-standard global config directory for recon.
///
/// - Linux/macOS: `~/.config/recon/`
/// - Windows:     `%APPDATA%\recon\`
///
/// Falls back to `./.recon` if the platform config dir is unavailable.
///
/// Can be overridden with the `RECON_CONFIG_DIR` environment variable,
/// which is useful in tests and CI environments that cannot write to the
/// real user config directory.
pub fn global_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("RECON_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("recon")
}

/// Validate a license, with local caching and offline fallback.
///
/// ## `key = None` (post-login / normal operation)
/// 1. Read `<cache_dir>/license.json` — return if cache is fresh (< 24 h) and signature valid.
/// 2. No valid cache → `Err("No valid license. Run 'recon login <key>'")`
///
/// ## `key = Some(k)` (login / explicit key)
/// 1. Try remote validation → verify server signature → cache the response on success.
/// 2. Remote unreachable → fall back to local cache if < 24 h old and signature valid.
/// 3. Both fail → `Err(…)`
///
/// In both cases, an expired subscription (`expires_at` in the past) is an error.
pub fn validate_license(key: Option<&str>, cache_dir: &Path) -> Result<ValidatedLicense, String> {
    let cache_path = cache_dir.join("license.json");

    let license = match key {
        // ── Cache-only path (normal operation after login) ────────────────────
        None | Some("") => match read_cache(&cache_path) {
            Some(cached) => {
                debug!("using cached license (no key supplied)");
                response_to_license(cached.response, LicenseSource::Cache)
            }
            None => {
                return Err(
                    "No valid license found. Run 'recon login <key>' to authenticate.\n\
                     Get a key at https://mcprecon.pages.dev/login"
                        .into(),
                );
            }
        },

        // ── Remote validation path (login / explicit key) ─────────────────────
        Some(k) => match validate_remote(k) {
            Ok(resp) => {
                if let Err(e) = write_cache(&cache_path, &resp) {
                    debug!("failed to cache license: {e}");
                }
                let lic = response_to_license(resp, LicenseSource::Remote);
                info!(
                    tier = lic.tier.name(),
                    source = %lic.source,
                    "license {}",
                    lic.message
                );
                lic
            }
            Err(e) => {
                warn!("remote license validation failed: {e}");
                match read_cache(&cache_path) {
                    Some(cached) => {
                        info!(
                            tier = cached.response.tier,
                            "using cached license (server unreachable)"
                        );
                        response_to_license(cached.response, LicenseSource::Cache)
                    }
                    None => {
                        return Err(format!(
                            "Could not validate API key: {e}\n\
                             Check your key at https://mcprecon.pages.dev/dashboard"
                        ));
                    }
                }
            }
        },
    };

    // Subscription expiry check — applies regardless of cache vs remote source.
    if license.expires_at > 0 && license.expires_at < now_secs() {
        return Err("License expired. Run 'recon login <key>' to renew.\n\
             Manage your subscription at https://mcprecon.pages.dev/dashboard"
            .into());
    }

    Ok(license)
}

/// Compute an HMAC-SHA256 signature over the canonical license payload.
///
/// Canonical form: `"{tier}:{max_repos}:{max_files}:{max_loc}:{expires_at}"`.
///
/// This function is `pub` so that e2e test helpers in other crates (e.g.
/// `recon-cli` integration tests) can generate valid fake licenses.
///
/// # Errors
/// Returns `Err` if the HMAC key cannot be initialized (should never happen
/// with the compile-time embedded key).
pub fn compute_signature(resp: &LicenseResponse) -> Result<String, String> {
    let payload = format!(
        "{}:{}:{}:{}:{}",
        resp.tier,
        resp.limits.max_repos,
        resp.limits.max_files,
        resp.limits.max_loc,
        resp.expires_at
    );
    let mut mac = new_mac().ok_or_else(|| "HMAC key initialization failed".to_string())?;
    mac.update(payload.as_bytes());
    Ok(mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Path to the credentials file (raw API key).
///
/// Separate from `license.json` so the signed auth result (durable,
/// HMAC-verified, safe to read without restrictions) stays independent
/// from the credential (sensitive, chmod-0600 on Unix, required only by
/// the periodic re-validation task).
pub fn credentials_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("credentials.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredCredentials {
    key: String,
}

/// Persist the raw API key next to the license cache. On Unix, the file is
/// written with mode 0600 so only the owning user can read it — same pattern
/// as `gh`, `aws-cli`, and `stripe` CLIs. On Windows, file-ACL defaults do
/// the equivalent via the user's profile ACL.
///
/// Idempotent — overwrites any previous credential.
pub fn save_credentials(cache_dir: &Path, key: &str) -> Result<(), String> {
    std::fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
    let path = credentials_path(cache_dir);
    let body = serde_json::to_string(&StoredCredentials {
        key: key.to_string(),
    })
    .map_err(|e| e.to_string())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| e.to_string())?;
        use std::io::Write;
        f.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, body).map_err(|e| e.to_string())?;
    }

    Ok(())
}

/// Read the raw API key from disk, if present. Returns `None` if the
/// credentials file is missing or unreadable — the caller treats this as
/// "no cached credential, run `recon login` to refresh remotely".
pub fn read_credentials(cache_dir: &Path) -> Option<String> {
    let path = credentials_path(cache_dir);
    let body = std::fs::read_to_string(&path).ok()?;
    let stored: StoredCredentials = serde_json::from_str(&body).ok()?;
    if stored.key.is_empty() {
        return None;
    }
    Some(stored.key)
}

/// Remove the credentials file. Idempotent — missing file is not an error.
/// Called by `recon logout` and by the periodic re-validation task when the
/// worker rejects the key (so subsequent polls don't hammer a dead key).
pub fn delete_credentials(cache_dir: &Path) -> Result<(), String> {
    let path = credentials_path(cache_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Delete the cached signed license.
///
/// Used together with [`delete_credentials`] on worker-initiated rejection so
/// that cold-starting `recon serve` after revocation fails cleanly with
/// "run `recon login`" rather than continuing to honor the stale cache
/// until its 24h TTL lapses.
pub fn delete_cache(cache_dir: &Path) -> Result<(), String> {
    let path = cache_dir.join("license.json");
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Write a signed Pro-tier dev license to `<cache_dir>/license.json`.
///
/// Useful in integration tests that need the CLI to start without a real API
/// key.  The signature is computed with the same dev HMAC key that is baked
/// into the binary when `RECON_LICENSE_HMAC_KEY` is not set at build time.
pub fn seed_dev_cache(cache_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
    let resp = LicenseResponse {
        valid: true,
        tier: "Pro".into(),
        limits: LimitsPayload {
            max_repos: 10,
            max_files: 5_000,
            max_loc: 200_000,
        },
        expires_at: 0,
        message: "dev license".into(),
        signature: None, // write_cache computes it
    };
    write_cache(&cache_dir.join("license.json"), &resp)
}

/// Why a remote validation call did not produce a trusted license.
///
/// The periodic re-validation task in `recon serve` distinguishes these so it
/// can propagate a dashboard-initiated revocation quickly — a `Rejected`
/// result means the worker has explicitly told us this key is no longer
/// honored and we should stop serving tool calls. `Transient` means the
/// network lied, so we keep the current cached license and retry later.
#[derive(Debug, Clone)]
pub enum RemoteError {
    /// Worker returned a definitive rejection (HTTP 4xx, or 200 with
    /// `valid: false`, or a signature that fails verification after the
    /// server claimed to sign it). The key should not be trusted further.
    Rejected(String),
    /// Transport, DNS, 5xx, or JSON-parse failure. Retry-safe.
    Transient(String),
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(m) => write!(f, "rejected: {m}"),
            Self::Transient(m) => write!(f, "transient: {m}"),
        }
    }
}

/// Call the remote license endpoint and return a result whose variant tells
/// the caller whether to treat a failure as a revoke signal or as a network
/// hiccup.
pub fn validate_remote_strict(key: &str) -> Result<LicenseResponse, RemoteError> {
    let api_url = std::env::var("RECON_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string());
    let url = format!("{api_url}/v1/license/validate");

    let mut response = match ureq::post(&url)
        .header("Authorization", &format!("Bearer {key}"))
        .header("User-Agent", concat!("recon/", env!("CARGO_PKG_VERSION")))
        .send_json(serde_json::json!({ "key": key }))
    {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(code)) if (400..500).contains(&code) => {
            return Err(RemoteError::Rejected(format!(
                "HTTP {code} from license endpoint"
            )));
        }
        Err(e) => return Err(RemoteError::Transient(format!("HTTP error: {e}"))),
    };

    let resp: LicenseResponse = response
        .body_mut()
        .read_json()
        .map_err(|e| RemoteError::Transient(format!("JSON parse error: {e}")))?;

    if !resp.valid {
        return Err(RemoteError::Rejected(format!(
            "worker rejected key: {}",
            resp.message
        )));
    }
    if !verify_signature(&resp) {
        return Err(RemoteError::Rejected(
            "server response signature invalid or missing".into(),
        ));
    }
    Ok(resp)
}

/// Call the remote license server and return a parsed, signature-verified response.
///
/// Thin wrapper over [`validate_remote_strict`] that collapses both failure
/// modes into a string for callers that don't care to distinguish them
/// (e.g. `recon login`, where a failure is always fatal).
fn validate_remote(key: &str) -> Result<LicenseResponse, String> {
    validate_remote_strict(key).map_err(|e| e.to_string())
}

/// Verify the HMAC-SHA256 signature on a license response.
///
/// Uses constant-time comparison to prevent timing side-channels.
fn verify_signature(resp: &LicenseResponse) -> bool {
    let sig_hex = match &resp.signature {
        Some(s) if !s.is_empty() => s,
        _ => return false,
    };
    let sig_bytes = match hex_decode(sig_hex) {
        Some(b) => b,
        None => return false,
    };
    let payload = format!(
        "{}:{}:{}:{}:{}",
        resp.tier,
        resp.limits.max_repos,
        resp.limits.max_files,
        resp.limits.max_loc,
        resp.expires_at
    );
    let mut mac = match new_mac() {
        Some(m) => m,
        None => return false,
    };
    mac.update(payload.as_bytes());
    mac.verify_slice(&sig_bytes).is_ok()
}

/// Decode a lowercase hex string into bytes. Returns `None` on invalid input.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = hex_nibble(pair[0])?;
            let lo = hex_nibble(pair[1])?;
            Some((hi << 4) | lo)
        })
        .collect()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Write a successfully-validated response into `license.json`.
///
/// Thin `pub` wrapper over the internal `write_cache` helper so the
/// periodic re-validation task in `recon serve` can refresh the disk
/// cache after a successful remote poll without reaching into
/// module-private plumbing.
pub fn write_cache_public(cache_dir: &Path, resp: &LicenseResponse) -> Result<(), String> {
    write_cache(&cache_dir.join("license.json"), resp)
}

/// `pub` wrapper over `response_to_license` for the same reason as above.
pub fn response_to_validated(resp: LicenseResponse, source: LicenseSource) -> ValidatedLicense {
    response_to_license(resp, source)
}

/// Convert an API response into a `ValidatedLicense`.
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
        revoked: false,
    }
}

/// Intern a tier name as a `&'static str`.
///
/// Safe because the set of tier names is small — at most a handful of
/// unique allocations over the lifetime of the process.
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

/// Persist a signed license response to disk.
///
/// The signature is (re-)computed from the local HMAC key before writing,
/// ensuring the on-disk cache is always self-consistent.
fn write_cache(path: &Path, resp: &LicenseResponse) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut resp = resp.clone();
    resp.signature = Some(compute_signature(&resp)?);
    let cached = CachedLicense {
        cached_at: now_secs(),
        response: resp,
    };
    let json = serde_json::to_string_pretty(&cached).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

/// Read and TTL-check the license cache.  Returns `None` if missing, corrupt,
/// older than [`CACHE_TTL_SECS`], or if the signature is absent or invalid.
fn read_cache(path: &Path) -> Option<CachedLicense> {
    let content = std::fs::read_to_string(path).ok()?;
    let cached: CachedLicense = serde_json::from_str(&content).ok()?;

    let age = now_secs().saturating_sub(cached.cached_at);
    if age > CACHE_TTL_SECS {
        debug!(age_hours = age / 3600, "license cache TTL expired");
        return None;
    }

    // Strict HMAC verification — reject tampered or unsigned caches.
    if !verify_signature(&cached.response) {
        debug!("license cache signature invalid or missing — cache rejected");
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
    use std::fs;
    use tempfile::tempdir;

    // ── helpers ────────────────────────────────────────────────────────────────

    fn make_resp(tier: &str, max_repos: usize, expires_at: u64) -> LicenseResponse {
        LicenseResponse {
            valid: true,
            tier: tier.into(),
            limits: LimitsPayload {
                max_repos,
                max_files: 5_000,
                max_loc: 200_000,
            },
            expires_at,
            message: format!("{tier} plan active"),
            signature: None, // seed_cache adds the correct signature
        }
    }

    /// Write a pre-built cache entry directly, bypassing the TTL check.
    /// The response is signed with the test HMAC key so `read_cache` accepts it.
    fn seed_cache(dir: &Path, resp: LicenseResponse, cached_at: u64) {
        let path = dir.join("license.json");
        let mut resp = resp;
        resp.signature = Some(compute_signature(&resp).unwrap());
        let entry = CachedLicense {
            cached_at,
            response: resp,
        };
        fs::write(&path, serde_json::to_string(&entry).unwrap()).unwrap();
    }

    // ── global_config_dir ──────────────────────────────────────────────────────

    #[test]
    fn global_config_dir_ends_with_recon() {
        let dir = global_config_dir();
        assert_eq!(
            dir.file_name().and_then(|n| n.to_str()),
            Some("recon"),
            "expected path to end in 'recon', got: {dir:?}"
        );
    }

    #[test]
    fn global_config_dir_is_absolute() {
        let dir = global_config_dir();
        if let Some(config) = dirs::config_dir() {
            assert!(
                dir.starts_with(config),
                "expected global_config_dir to be under config_dir, got: {dir:?}"
            );
        }
    }

    // ── key = None / "" — cache-only path ─────────────────────────────────────

    #[test]
    fn no_key_no_cache_errors_with_login_hint() {
        let dir = tempdir().unwrap();
        let err = validate_license(None, dir.path()).unwrap_err();
        assert!(
            err.contains("recon login"),
            "expected login hint, got: {err}"
        );
    }

    #[test]
    fn empty_key_no_cache_errors_with_login_hint() {
        let dir = tempdir().unwrap();
        let err = validate_license(Some(""), dir.path()).unwrap_err();
        assert!(
            err.contains("recon login"),
            "expected login hint, got: {err}"
        );
    }

    #[test]
    fn no_key_fresh_cache_returns_ok() {
        let dir = tempdir().unwrap();
        seed_cache(dir.path(), make_resp("Pro", 10, 0), now_secs());
        let lic = validate_license(None, dir.path()).unwrap();
        assert_eq!(lic.tier.name(), "Pro");
        assert_eq!(lic.source, LicenseSource::Cache);
        assert_eq!(lic.tier.limits().max_repos, 10);
    }

    #[test]
    fn empty_key_fresh_cache_returns_ok() {
        let dir = tempdir().unwrap();
        seed_cache(dir.path(), make_resp("Team", 25, 0), now_secs());
        let lic = validate_license(Some(""), dir.path()).unwrap();
        assert_eq!(lic.tier.name(), "Team");
        assert_eq!(lic.source, LicenseSource::Cache);
    }

    #[test]
    fn no_key_stale_cache_errors_with_login_hint() {
        let dir = tempdir().unwrap();
        // cached_at = 0 → age > 24h → stale
        seed_cache(dir.path(), make_resp("Pro", 10, 0), 0);
        let err = validate_license(None, dir.path()).unwrap_err();
        assert!(
            err.contains("recon login"),
            "stale cache should prompt login: {err}"
        );
    }

    #[test]
    fn no_key_cache_just_within_ttl_is_valid() {
        let dir = tempdir().unwrap();
        // cached 1 second before TTL would expire
        let cached_at = now_secs() - (CACHE_TTL_SECS - 1);
        seed_cache(dir.path(), make_resp("Pro", 10, 0), cached_at);
        assert!(validate_license(None, dir.path()).is_ok());
    }

    #[test]
    fn no_key_cache_exactly_at_ttl_boundary_is_stale() {
        let dir = tempdir().unwrap();
        let cached_at = now_secs().saturating_sub(CACHE_TTL_SECS + 1);
        seed_cache(dir.path(), make_resp("Pro", 10, 0), cached_at);
        let err = validate_license(None, dir.path()).unwrap_err();
        assert!(err.contains("recon login"), "got: {err}");
    }

    // ── HMAC tamper detection ─────────────────────────────────────────────────

    #[test]
    fn unsigned_cache_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("license.json");
        // Write a response without a signature field
        let entry = CachedLicense {
            cached_at: now_secs(),
            response: make_resp("Pro", 10, 0), // signature: None
        };
        fs::write(&path, serde_json::to_string(&entry).unwrap()).unwrap();
        let err = validate_license(None, dir.path()).unwrap_err();
        assert!(
            err.contains("recon login"),
            "unsigned cache should be rejected: {err}"
        );
    }

    #[test]
    fn tampered_tier_in_cache_is_rejected() {
        let dir = tempdir().unwrap();
        // Write a signed Pro response, then manually edit to Enterprise.
        seed_cache(dir.path(), make_resp("Pro", 10, 0), now_secs());
        let path = dir.path().join("license.json");
        let content = fs::read_to_string(&path).unwrap();
        let tampered = content.replace("\"Pro\"", "\"Enterprise\"");
        fs::write(&path, tampered).unwrap();
        let err = validate_license(None, dir.path()).unwrap_err();
        assert!(
            err.contains("recon login"),
            "tampered cache should be rejected: {err}"
        );
    }

    #[test]
    fn compute_signature_is_deterministic() {
        let resp = make_resp("Pro", 10, 0);
        let sig1 = compute_signature(&resp).unwrap();
        let sig2 = compute_signature(&resp).unwrap();
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn compute_signature_changes_with_tier() {
        let pro = compute_signature(&make_resp("Pro", 10, 0)).unwrap();
        let ent = compute_signature(&make_resp("Enterprise", 10, 0)).unwrap();
        assert_ne!(pro, ent);
    }

    // ── subscription expiry ───────────────────────────────────────────────────

    #[test]
    fn expired_subscription_in_cache_is_rejected() {
        let dir = tempdir().unwrap();
        let past = now_secs() - 3_600; // expired 1 hour ago
        seed_cache(dir.path(), make_resp("Pro", 10, past), now_secs());
        let err = validate_license(None, dir.path()).unwrap_err();
        assert!(err.contains("expired"), "got: {err}");
        assert!(err.contains("recon login"), "should hint at renewal: {err}");
    }

    #[test]
    fn future_expiry_is_ok() {
        let dir = tempdir().unwrap();
        let future = now_secs() + 86_400;
        seed_cache(dir.path(), make_resp("Pro", 10, future), now_secs());
        let lic = validate_license(None, dir.path()).unwrap();
        assert_eq!(lic.tier.name(), "Pro");
        assert_eq!(lic.expires_at, future);
    }

    #[test]
    fn zero_expires_at_means_no_expiry() {
        let dir = tempdir().unwrap();
        seed_cache(dir.path(), make_resp("Enterprise", 1_000, 0), now_secs());
        let lic = validate_license(None, dir.path()).unwrap();
        assert_eq!(lic.tier.name(), "Enterprise");
    }

    #[test]
    fn expiry_one_second_from_now_is_ok() {
        let dir = tempdir().unwrap();
        let barely_future = now_secs() + 1;
        seed_cache(dir.path(), make_resp("Pro", 10, barely_future), now_secs());
        assert!(validate_license(None, dir.path()).is_ok());
    }

    // ── key = Some(k) — remote path with cache fallback ──────────────────────

    #[test]
    fn invalid_key_no_cache_returns_error() {
        let dir = tempdir().unwrap();
        let err = validate_license(Some("sk-invalid-key-12345"), dir.path()).unwrap_err();
        assert!(
            err.contains("Could not validate") || err.contains("HTTP error"),
            "got: {err}"
        );
    }

    #[test]
    fn invalid_key_with_fresh_cache_falls_back() {
        let dir = tempdir().unwrap();
        seed_cache(dir.path(), make_resp("Pro", 10, 0), now_secs());
        // Remote will fail; fresh cache should be used as fallback
        let lic = validate_license(Some("sk-bad-key-fallback-test"), dir.path()).unwrap();
        assert_eq!(lic.tier.name(), "Pro");
        assert_eq!(lic.source, LicenseSource::Cache);
    }

    #[test]
    fn invalid_key_with_stale_cache_returns_error() {
        let dir = tempdir().unwrap();
        // Remote fails + stale cache = error
        seed_cache(dir.path(), make_resp("Pro", 10, 0), 0);
        let err = validate_license(Some("sk-bad-key-stale-cache"), dir.path()).unwrap_err();
        assert!(
            err.contains("Could not validate") || err.contains("HTTP error"),
            "got: {err}"
        );
    }

    // ── cache helpers — unit ──────────────────────────────────────────────────

    #[test]
    fn write_and_read_cache_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("license.json");
        let resp = make_resp("Pro", 50, now_secs() + 86_400);
        write_cache(&path, &resp).unwrap();

        let cached = read_cache(&path).unwrap();
        assert_eq!(cached.response.tier, "Pro");
        assert_eq!(cached.response.limits.max_repos, 50);
    }

    #[test]
    fn write_cache_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("license.json");
        write_cache(&nested, &make_resp("Free", 1, 0)).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn write_cache_embeds_signature() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("license.json");
        write_cache(&path, &make_resp("Pro", 10, 0)).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("signature"),
            "write_cache must embed signature"
        );
    }

    #[test]
    fn read_cache_returns_none_for_missing_file() {
        let dir = tempdir().unwrap();
        assert!(read_cache(&dir.path().join("license.json")).is_none());
    }

    #[test]
    fn read_cache_returns_none_for_corrupt_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("license.json");
        fs::write(&path, b"not valid json {{{").unwrap();
        assert!(read_cache(&path).is_none());
    }

    #[test]
    fn read_cache_returns_none_for_ttl_expired() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("license.json");
        let mut resp = make_resp("Pro", 10, 0);
        resp.signature = Some(compute_signature(&resp).unwrap());
        let entry = CachedLicense {
            cached_at: 0, // epoch → ancient
            response: resp,
        };
        fs::write(&path, serde_json::to_string(&entry).unwrap()).unwrap();
        assert!(read_cache(&path).is_none());
    }

    #[test]
    fn cache_preserves_all_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("license.json");
        let resp = LicenseResponse {
            valid: true,
            tier: "Team".into(),
            limits: LimitsPayload {
                max_repos: 25,
                max_files: 50_000,
                max_loc: 4_000_000,
            },
            expires_at: 9_999_999_999,
            message: "Team plan rocks".into(),
            signature: None,
        };
        write_cache(&path, &resp).unwrap();
        let cached = read_cache(&path).unwrap();
        assert_eq!(cached.response.limits.max_files, 50_000);
        assert_eq!(cached.response.limits.max_loc, 4_000_000);
        assert_eq!(cached.response.expires_at, 9_999_999_999);
        assert_eq!(cached.response.message, "Team plan rocks");
    }

    // ── seed_dev_cache ────────────────────────────────────────────────────────

    #[test]
    fn seed_dev_cache_creates_valid_license() {
        let dir = tempdir().unwrap();
        seed_dev_cache(dir.path()).unwrap();
        let lic = validate_license(None, dir.path()).unwrap();
        assert_eq!(lic.tier.name(), "Pro");
        assert_eq!(lic.source, LicenseSource::Cache);
    }

    // ── tier name interning ───────────────────────────────────────────────────

    #[test]
    fn leak_tier_name_known_variants() {
        assert_eq!(leak_tier_name("free"), "Free");
        assert_eq!(leak_tier_name("FREE"), "Free");
        assert_eq!(leak_tier_name("pro"), "Pro");
        assert_eq!(leak_tier_name("Pro"), "Pro");
        assert_eq!(leak_tier_name("team"), "Team");
        assert_eq!(leak_tier_name("enterprise"), "Enterprise");
        assert_eq!(leak_tier_name("uncapped"), "Uncapped");
    }

    #[test]
    fn leak_tier_name_unknown_preserved_as_is() {
        assert_eq!(leak_tier_name("platinum"), "platinum");
        assert_eq!(leak_tier_name("CustomTier"), "CustomTier");
    }

    // ── LicenseSource display ─────────────────────────────────────────────────

    #[test]
    fn license_source_display_strings() {
        assert_eq!(LicenseSource::Remote.to_string(), "validated");
        assert_eq!(LicenseSource::Cache.to_string(), "cached");
        assert_eq!(LicenseSource::FreeTier.to_string(), "free tier");
    }

    // ── validated license fields ──────────────────────────────────────────────

    #[test]
    fn validated_license_limits_accessible() {
        let dir = tempdir().unwrap();
        seed_cache(dir.path(), make_resp("Pro", 10, 0), now_secs());
        let lic = validate_license(None, dir.path()).unwrap();
        let limits = lic.tier.limits();
        assert_eq!(limits.max_repos, 10);
        assert_eq!(limits.max_files, 5_000);
        assert_eq!(limits.max_loc, 200_000);
    }

    #[test]
    fn validated_license_message_preserved() {
        let dir = tempdir().unwrap();
        let mut resp = make_resp("Pro", 10, 0);
        resp.message = "Hello from server".into();
        seed_cache(dir.path(), resp, now_secs());
        let lic = validate_license(None, dir.path()).unwrap();
        assert_eq!(lic.message, "Hello from server");
    }
}
