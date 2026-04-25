//! Remote account (repo registry) client.
//!
//! Talks to the worker's `/v1/account/repos` endpoints from the CLI side.
//! Mirrors the [`crate::license`] module's `ureq + rustls` shape so
//! cross-compile targets stay aligned (no native-tls / openssl-sys).
//!
//! Endpoints exercised:
//! - `POST /v1/account/repos`           — register a repo (atomic, may 403)
//! - `GET  /v1/account/repos`           — list user's registered repos
//! - `DELETE /v1/account/repos/:fp`     — release a slot
//! - `GET  /v1/health`                  — used by `recon doctor`
//!
//! All functions are sync (`ureq` is sync); the CLI calls them from blocking
//! contexts (`recon init`, `recon repos {list,remove}`, `recon doctor`),
//! never from inside the MCP server's async request path.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::Path;
use thiserror::Error;

/// Default worker base URL. Override with `RECON_API_URL`.
///
/// Kept in sync with [`crate::license::DEFAULT_API_URL`] so a single
/// override env var swaps the whole CLI to a staging worker.
const DEFAULT_API_URL: &str = "https://recon-api.kumarashutosh34169.workers.dev";

/// Failure modes for a remote account call.
///
/// Each variant carries enough context for the CLI to render a useful
/// error without the caller fishing through a single `String`. The split
/// between `Network` (transient — try again) and `Server` (5xx, propagate)
/// matches what the license module distinguishes between `Transient` and
/// `Rejected`, so callers can implement the same retry semantics.
#[derive(Debug, Error)]
pub enum AccountError {
    /// Could not reach the worker at all (DNS, TCP, TLS, timeout).
    #[error("could not reach recon worker: {0}")]
    Network(String),
    /// 401 — API key missing, unknown, revoked, or expired.
    #[error("api key rejected: {0}")]
    Unauthorized(String),
    /// 403 — registering this fingerprint would push the user past `max_repos`.
    #[error("repo quota exceeded: {message}")]
    OverQuota {
        /// The user's tier-defined `max_repos`.
        limit: u32,
        /// The api_key's tier (`Free`, `Pro`, `Team`, …).
        tier: String,
        /// Server-provided user-facing message.
        message: String,
    },
    /// 404 — fingerprint not registered (DELETE only).
    #[error("fingerprint not registered")]
    NotFound,
    /// 400 — request body or path-param failed worker-side validation.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// 5xx — worker error.
    #[error("worker error: {0}")]
    Server(String),
    /// Response body wasn't the JSON shape we expected.
    #[error("response parse error: {0}")]
    BadResponse(String),
}

/// POST /v1/account/repos response (success cases — 201 / 200).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    /// SHA-256 fingerprint of the repo's canonical absolute path.
    pub fingerprint: String,
    /// `"registered"` for first-time, `"refreshed"` for repeat POST.
    pub status: String,
    /// Tier-defined `max_repos`.
    pub limit: u32,
    /// Tier name (`Free`, `Pro`, …).
    pub tier: String,
}

/// GET /v1/account/repos response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResponse {
    /// Registered repos, server-sorted by `last_seen_at DESC`.
    pub repos: Vec<RemoteRepo>,
    /// Tier-defined `max_repos`.
    pub limit: u32,
    /// Tier name (`Free`, `Pro`, …).
    pub tier: String,
}

/// Single repo entry returned by the worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteRepo {
    /// SHA-256 fingerprint (lowercase hex).
    pub fingerprint: String,
    /// ISO-8601 timestamp of first registration.
    pub first_seen_at: String,
    /// ISO-8601 timestamp of most recent registration touch.
    pub last_seen_at: String,
}

/// Worker error envelope returned on 4xx / 5xx.
#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: Option<String>,
    message: Option<String>,
    limit: Option<u32>,
    tier: Option<String>,
}

/// Resolve the worker base URL (env override → default).
fn api_base_url() -> String {
    std::env::var("RECON_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string())
}

/// Compute the SHA-256 fingerprint of a path, matching the worker's
/// `^[0-9a-f]{64}$` validator.
///
/// Tries `canonicalize` first so two CLIs invoked from different cwds
/// produce the same fingerprint. Falls back to the verbatim path string
/// when canonicalize fails — which it does for paths the user is about
/// to delete (covering `recon repos remove /old/deleted/dir`) and for
/// paths inside symlink loops the OS refuses to resolve. The fallback
/// is a feature, not a workaround: enforcing canonicalize would brick
/// the cleanup path.
pub fn fingerprint_path(path: &Path) -> String {
    let s = match path.canonicalize() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    };
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex_encode(hasher.finalize().as_slice())
}

/// Lowercase hex encoding for fingerprints. The recon-server crate
/// already pulls in sha2 + thiserror; pulling in `hex` for this single
/// site would add a dep just to save four lines.
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// POST /v1/account/repos — register a repo.
///
/// Atomic on the worker side: even at limit-1, two concurrent calls
/// with distinct fingerprints can't both win. Idempotent: a repeat call
/// with the same fingerprint returns `status: "refreshed"`.
pub fn register_repo(api_key: &str, fingerprint: &str) -> Result<RegisterResponse, AccountError> {
    let url = format!("{}/v1/account/repos", api_base_url());
    let mut response = match ureq::post(&url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("User-Agent", concat!("recon/", env!("CARGO_PKG_VERSION")))
        .send_json(serde_json::json!({ "fingerprint": fingerprint }))
    {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(code)) => {
            return Err(translate_status_for_register(
                code,
                &url,
                api_key,
                fingerprint,
            ))
        }
        Err(e) => return Err(AccountError::Network(e.to_string())),
    };
    response
        .body_mut()
        .read_json::<RegisterResponse>()
        .map_err(|e| AccountError::BadResponse(e.to_string()))
}

/// Re-issue the original POST when we hit a non-2xx, capture the body,
/// and translate to a typed error. ureq's `StatusCode(code)` doesn't
/// give us the response body, so the only way to get the worker's JSON
/// envelope is to retry with a manual response handler. This is fine
/// because the only race-relevant call (the parallel POST) is already
/// guarded server-side.
fn translate_status_for_register(
    code: u16,
    url: &str,
    api_key: &str,
    fingerprint: &str,
) -> AccountError {
    let body = match ureq::post(url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("User-Agent", concat!("recon/", env!("CARGO_PKG_VERSION")))
        .config()
        .http_status_as_error(false)
        .build()
        .send_json(serde_json::json!({ "fingerprint": fingerprint }))
    {
        Ok(mut r) => r.body_mut().read_json::<ErrorEnvelope>().ok(),
        Err(_) => None,
    };
    translate_status(code, body)
}

/// GET /v1/account/repos — list the user's registered repos.
pub fn list_repos(api_key: &str) -> Result<ListResponse, AccountError> {
    let url = format!("{}/v1/account/repos", api_base_url());
    let mut response = match ureq::get(&url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("User-Agent", concat!("recon/", env!("CARGO_PKG_VERSION")))
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(code)) => return Err(translate_status(code, None)),
        Err(e) => return Err(AccountError::Network(e.to_string())),
    };
    response
        .body_mut()
        .read_json::<ListResponse>()
        .map_err(|e| AccountError::BadResponse(e.to_string()))
}

/// DELETE /v1/account/repos/:fingerprint — release a slot.
pub fn unregister_repo(api_key: &str, fingerprint: &str) -> Result<(), AccountError> {
    let url = format!(
        "{}/v1/account/repos/{}",
        api_base_url(),
        urlencode_segment(fingerprint)
    );
    match ureq::delete(&url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("User-Agent", concat!("recon/", env!("CARGO_PKG_VERSION")))
        .call()
    {
        Ok(_) => Ok(()),
        Err(ureq::Error::StatusCode(code)) => Err(translate_status(code, None)),
        Err(e) => Err(AccountError::Network(e.to_string())),
    }
}

/// GET /v1/health — used by `recon doctor`. Returns Ok on any 2xx,
/// regardless of body shape, so a future health-payload change can't
/// break the doctor.
pub fn ping_health() -> Result<(), AccountError> {
    let url = format!("{}/v1/health", api_base_url());
    match ureq::get(&url)
        .header("User-Agent", concat!("recon/", env!("CARGO_PKG_VERSION")))
        .call()
    {
        Ok(_) => Ok(()),
        Err(ureq::Error::StatusCode(code)) => Err(AccountError::Server(format!(
            "health endpoint returned {code}"
        ))),
        Err(e) => Err(AccountError::Network(e.to_string())),
    }
}

/// Map an HTTP status code (and optional worker error body) to an
/// [`AccountError`] variant.
///
/// Splits 4xx into authentication / quota / not-found / generic-bad-request
/// so callers can render different UX. 5xx collapses to `Server` since
/// the CLI cannot disambiguate further.
fn translate_status(code: u16, body: Option<ErrorEnvelope>) -> AccountError {
    match code {
        401 => AccountError::Unauthorized(
            body.and_then(|b| b.error)
                .unwrap_or_else(|| "no detail".into()),
        ),
        403 => AccountError::OverQuota {
            limit: body.as_ref().and_then(|b| b.limit).unwrap_or(0),
            tier: body
                .as_ref()
                .and_then(|b| b.tier.clone())
                .unwrap_or_else(|| "Free".into()),
            message: body
                .and_then(|b| b.message.or(b.error))
                .unwrap_or_else(|| "max_repos exceeded".into()),
        },
        404 => AccountError::NotFound,
        400 => AccountError::BadRequest(
            body.and_then(|b| b.error)
                .unwrap_or_else(|| "no detail".into()),
        ),
        c if (500..600).contains(&c) => AccountError::Server(format!("HTTP {c}")),
        c => AccountError::Server(format!("unexpected HTTP {c}")),
    }
}

/// Minimal path-segment encoder.
///
/// Fingerprints are 64-char lowercase hex so URL-encoding is a no-op
/// today; this is here to harden against a future schema change that
/// allows other characters.
fn urlencode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => write!(out, "%{other:02X}").unwrap(),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn fingerprint_is_64_char_lowercase_hex() {
        let fp = fingerprint_path(&PathBuf::from(
            "/some/path/that/almost/certainly/does/not/exist",
        ));
        assert_eq!(fp.len(), 64);
        assert!(fp
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn fingerprint_stable_for_same_input() {
        let p = PathBuf::from("/no/such/path/recon-test");
        assert_eq!(fingerprint_path(&p), fingerprint_path(&p));
    }

    #[test]
    fn fingerprint_different_for_different_paths() {
        let a = fingerprint_path(&PathBuf::from("/a"));
        let b = fingerprint_path(&PathBuf::from("/b"));
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_canonicalises_when_possible() {
        // tempdir gives us a real path; canonicalize will succeed and
        // strip any trailing-slash / .. variants.
        let dir = tempfile::tempdir().unwrap();
        let with_slash = dir.path().join(".");
        let plain = dir.path().to_path_buf();
        assert_eq!(fingerprint_path(&with_slash), fingerprint_path(&plain));
    }

    #[test]
    fn fingerprint_falls_back_for_nonexistent_path() {
        // canonicalize fails → fallback to verbatim string. Distinct
        // verbatim strings → distinct fingerprints.
        let a = fingerprint_path(&PathBuf::from("/definitely/not/here/A"));
        let b = fingerprint_path(&PathBuf::from("/definitely/not/here/B"));
        assert_ne!(a, b);
    }

    #[test]
    fn translate_status_maps_known_codes() {
        let body = Some(ErrorEnvelope {
            error: Some("over quota".into()),
            message: Some("Pro plan allows 5 repos".into()),
            limit: Some(5),
            tier: Some("Pro".into()),
        });
        let err = translate_status(403, body);
        match err {
            AccountError::OverQuota {
                limit,
                tier,
                message,
            } => {
                assert_eq!(limit, 5);
                assert_eq!(tier, "Pro");
                assert!(message.contains("Pro"));
            }
            other => panic!("expected OverQuota, got {other:?}"),
        }

        assert!(matches!(
            translate_status(401, None),
            AccountError::Unauthorized(_)
        ));
        assert!(matches!(
            translate_status(404, None),
            AccountError::NotFound
        ));
        assert!(matches!(
            translate_status(400, None),
            AccountError::BadRequest(_)
        ));
        assert!(matches!(
            translate_status(500, None),
            AccountError::Server(_)
        ));
        assert!(matches!(
            translate_status(599, None),
            AccountError::Server(_)
        ));
        assert!(matches!(
            translate_status(418, None),
            AccountError::Server(_)
        ));
    }

    #[test]
    fn urlencode_segment_passes_hex_through() {
        let fp = "a".repeat(64);
        assert_eq!(urlencode_segment(&fp), fp);
    }

    #[test]
    fn urlencode_segment_escapes_unsafe() {
        assert_eq!(urlencode_segment("a/b"), "a%2Fb");
        assert_eq!(urlencode_segment("a b"), "a%20b");
    }
}
