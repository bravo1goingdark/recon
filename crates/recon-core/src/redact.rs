//! Secret redaction for tool responses.
//!
//! Scans text for common secret patterns and replaces them.
//! Also blocks sensitive file paths by default.

use std::path::Path;
use std::sync::OnceLock;

const REDACTED: &str = "***REDACTED***";

/// Sensitive file path patterns that are blocked by default.
const BLOCKED_PATHS: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    ".env.development",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
];

const BLOCKED_EXTENSIONS: &[&str] = &["pem", "key", "p12", "pfx", "jks"];

/// Secret patterns as (label, prefix, min_length).
const SECRET_PATTERNS: &[(&str, &str, usize)] = &[
    ("AWS Access Key", "AKIA", 20),
    ("AWS Secret Key", "aws_secret_access_key", 0),
    ("GitHub Token", "ghp_", 40),
    ("GitHub Token", "gho_", 40),
    ("GitHub Token", "ghs_", 40),
    ("GitHub Token", "github_pat_", 40),
    ("OpenAI Key", "sk-", 40),
    ("Anthropic Key", "sk-ant-", 40),
    ("Slack Token", "xoxb-", 40),
    ("Slack Token", "xoxp-", 40),
    ("Slack Token", "xapp-", 40),
];

/// PEM header markers.
const PEM_MARKERS: &[&str] = &[
    "-----BEGIN RSA PRIVATE KEY-----",
    "-----BEGIN PRIVATE KEY-----",
    "-----BEGIN EC PRIVATE KEY-----",
    "-----BEGIN DSA PRIVATE KEY-----",
    "-----BEGIN OPENSSH PRIVATE KEY-----",
];

/// Aho-Corasick automaton for fast pre-screening of all secret patterns + PEM markers.
/// Built once, reused across all calls. Returns `None` if construction fails (logged as error).
/// If no pattern matches, we skip the full scan entirely.
fn secret_scanner() -> Option<&'static aho_corasick::AhoCorasick> {
    static AC: OnceLock<Option<aho_corasick::AhoCorasick>> = OnceLock::new();
    AC.get_or_init(|| {
        let mut patterns: Vec<&str> = Vec::with_capacity(PEM_MARKERS.len() + SECRET_PATTERNS.len());
        patterns.extend(PEM_MARKERS.iter());
        for &(_label, prefix, _min_len) in SECRET_PATTERNS {
            patterns.push(prefix);
        }
        match aho_corasick::AhoCorasick::new(patterns) {
            Ok(ac) => Some(ac),
            Err(e) => {
                tracing::error!("failed to build secret scanner automaton: {e}");
                None
            }
        }
    })
    .as_ref()
}

/// Check if a path should be blocked from being served.
pub fn is_blocked_path(path: &Path) -> bool {
    let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or("");

    // Check blocked filenames — no format!() allocation
    for blocked in BLOCKED_PATHS {
        if filename == *blocked {
            return true;
        }
        // Check for "blocked.xxx" pattern without allocating
        if filename.len() > blocked.len()
            && filename.starts_with(blocked)
            && filename.as_bytes()[blocked.len()] == b'.'
        {
            return true;
        }
    }

    // Check blocked extensions
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if BLOCKED_EXTENSIONS.contains(&ext) {
            return true;
        }
    }

    false
}

/// Redact secrets from a string, returning the redacted version.
/// Returns None if no redaction was needed.
///
/// Uses Aho-Corasick for O(n) fast-path pre-screening: if no secret
/// prefix is found, returns None immediately without cloning the string.
/// Falls back to direct pattern scanning if the automaton is unavailable.
pub fn redact_secrets(text: &str) -> Option<String> {
    // Fast-path: single-pass pre-screen — if no pattern prefix found, skip entirely.
    // If the scanner failed to build, skip the fast path and always run the slow scan.
    if let Some(ac) = secret_scanner() {
        if !ac.is_match(text) {
            return None;
        }
    }

    // Slow path: AC found a prefix (or scanner unavailable) — verify with full pattern matching.
    // String is cloned here; if no real match is found, we return None (no cost to caller).
    let mut redacted = String::from(text);
    let mut changed = false;

    // Check for PEM private key blocks
    for marker in PEM_MARKERS {
        if let Some(start) = redacted.find(marker) {
            let end_marker = marker.replace("BEGIN", "END");
            if let Some(end) = redacted[start..].find(&end_marker) {
                let block_end = start + end + end_marker.len();
                redacted.replace_range(start..block_end, REDACTED);
                changed = true;
            }
        }
    }

    // Check for known secret prefixes
    for &(_label, prefix, min_len) in SECRET_PATTERNS {
        while let Some(pos) = redacted.find(prefix) {
            // Find the end of the token (whitespace, quote, or EOL)
            let remaining = &redacted[pos..];
            let token_end = remaining
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '`' || c == ',')
                .unwrap_or(remaining.len());

            if min_len == 0 || token_end >= min_len {
                redacted.replace_range(pos..pos + token_end, REDACTED);
                changed = true;
            } else {
                break; // Not a real match, stop searching for this pattern
            }
        }
    }

    if changed {
        Some(redacted)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_env_files() {
        assert!(is_blocked_path(Path::new(".env")));
        assert!(is_blocked_path(Path::new(".env.local")));
        assert!(is_blocked_path(Path::new(".env.production")));
        assert!(is_blocked_path(Path::new("config/.env")));
    }

    #[test]
    fn blocks_key_files() {
        assert!(is_blocked_path(Path::new("server.pem")));
        assert!(is_blocked_path(Path::new("private.key")));
        assert!(is_blocked_path(Path::new("id_rsa")));
        assert!(is_blocked_path(Path::new("id_ed25519")));
    }

    #[test]
    fn allows_normal_files() {
        assert!(!is_blocked_path(Path::new("src/main.rs")));
        assert!(!is_blocked_path(Path::new("README.md")));
        assert!(!is_blocked_path(Path::new("config.toml")));
    }

    #[test]
    fn redacts_aws_key() {
        let text = r#"AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE"#;
        let result = redact_secrets(text);
        assert!(result.is_some());
        let redacted = result.unwrap();
        assert!(redacted.contains(REDACTED));
        assert!(!redacted.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn redacts_github_token() {
        let text = r#"token: "ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx1234""#;
        let result = redact_secrets(text);
        assert!(result.is_some());
        assert!(result.unwrap().contains(REDACTED));
    }

    #[test]
    fn redacts_pem_block() {
        let text =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQ...\n-----END RSA PRIVATE KEY-----";
        let result = redact_secrets(text);
        assert!(result.is_some());
        let redacted = result.unwrap();
        assert!(!redacted.contains("MIIEpAIBAAKCAQ"));
    }

    #[test]
    fn no_redaction_needed() {
        let text = "fn main() { println!(\"hello\"); }";
        assert!(redact_secrets(text).is_none());
    }

    #[test]
    fn redacts_openai_key() {
        let text = "OPENAI_API_KEY=sk-proj-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        let result = redact_secrets(text);
        assert!(result.is_some());
        assert!(result.unwrap().contains(REDACTED));
    }

    #[test]
    fn fast_path_skips_clean_text() {
        // Typical code with no secrets — should hit the Aho-Corasick fast path
        let text = "fn process_data(input: &[u8]) -> Vec<u8> { input.to_vec() }";
        assert!(redact_secrets(text).is_none());
    }

    #[test]
    fn is_blocked_path_no_alloc() {
        // Regression: ensure .env.xxx patterns work without format!()
        assert!(is_blocked_path(Path::new(".env.staging")));
        assert!(!is_blocked_path(Path::new(".envrc")));
    }

    #[test]
    fn scanner_is_available() {
        // Ensure the AhoCorasick automaton builds successfully at runtime.
        assert!(
            secret_scanner().is_some(),
            "secret scanner must build from static patterns"
        );
    }

    #[test]
    fn redacts_anthropic_key() {
        let text = "key = sk-ant-api03-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        let result = redact_secrets(text);
        assert!(result.is_some());
        assert!(result.unwrap().contains(REDACTED));
    }

    #[test]
    fn redacts_slack_token() {
        let text = "SLACK_BOT_TOKEN=xoxb-xxxxxxxxxxxx-xxxxxxxxxxxx-xxxxxxxxxxxxxxxxxxxxxxxx";
        let result = redact_secrets(text);
        assert!(result.is_some());
        assert!(result.unwrap().contains(REDACTED));
    }

    #[test]
    fn no_false_positive_on_sk_prefix() {
        // "sk-" shorter than min_len=40 should not be redacted
        let text = "color: sk-blue";
        let result = redact_secrets(text);
        assert!(result.is_none(), "short sk- token must not be redacted");
    }

    #[test]
    fn multiple_secrets_in_one_string() {
        let text = "AKIAIOSFODNN7EXAMPLE and ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx1234";
        let result = redact_secrets(text);
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(!r.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!r.contains("ghp_xxxx"));
    }
}
