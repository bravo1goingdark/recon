//! Secret redaction for tool responses.
//!
//! Scans text for common secret patterns and replaces them.
//! Also blocks sensitive file paths by default.

use std::path::Path;

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

/// Check if a path should be blocked from being served.
pub fn is_blocked_path(path: &Path) -> bool {
    let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or("");

    // Check blocked filenames
    for blocked in BLOCKED_PATHS {
        if filename == *blocked || filename.starts_with(&format!("{blocked}.")) {
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
pub fn redact_secrets(text: &str) -> Option<String> {
    let mut redacted = String::from(text);
    let mut changed = false;

    // Check for PEM private key blocks
    for marker in PEM_MARKERS {
        if redacted.contains(marker) {
            // Replace the entire PEM block
            if let Some(start) = redacted.find(marker) {
                let end_marker = marker.replace("BEGIN", "END");
                if let Some(end) = redacted[start..].find(&end_marker) {
                    let block_end = start + end + end_marker.len();
                    redacted.replace_range(start..block_end, REDACTED);
                    changed = true;
                }
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
}
