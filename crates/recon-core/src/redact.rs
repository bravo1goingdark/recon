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

/// Pre-computed PEM (header, end_marker) pairs — built once via `LazyLock`.
static PEM_PAIRS: std::sync::LazyLock<Vec<(&'static str, String)>> =
    std::sync::LazyLock::new(|| {
        PEM_MARKERS
            .iter()
            .map(|&m| (m, m.replace("BEGIN", "END")))
            .collect()
    });

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

/// Returns true when a single path component (file or directory name) matches
/// a blocked filename, a `<blocked>.<anything>` pattern, or carries a blocked
/// extension.
///
/// Centralised so `is_blocked_path` can apply the same rules to every
/// component of a path, not only the leaf — otherwise paths like
/// `vault/.pem/leaf.txt` slip through (the leaf has no blocked extension,
/// but an intermediate directory is itself a sensitive bucket).
fn is_blocked_component(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }

    // Exact-match or `<blocked>.<anything>` against the BLOCKED_PATHS list.
    for blocked in BLOCKED_PATHS {
        if name == *blocked {
            return true;
        }
        if name.len() > blocked.len()
            && name.starts_with(blocked)
            && name.as_bytes()[blocked.len()] == b'.'
        {
            return true;
        }
    }

    // Extension check. `Path::extension` is leaf-only, so for intermediate
    // components we extract the suffix manually to keep behaviour uniform.
    if let Some(dot) = name.rfind('.') {
        // Skip dotfiles like ".env" — they're handled by BLOCKED_PATHS above.
        if dot > 0 {
            let ext = &name[dot + 1..];
            if BLOCKED_EXTENSIONS.contains(&ext) {
                return true;
            }
        }
    }

    false
}

/// Check if a path should be blocked from being served.
///
/// Matches against every path component, not only the file name, so that
/// paths like `secrets/.env/dump.json` or `vault/key.pem/notes.md` are
/// caught even when the leaf itself looks innocuous.
pub fn is_blocked_path(path: &Path) -> bool {
    for component in path.components() {
        if let std::path::Component::Normal(os) = component {
            if let Some(name) = os.to_str() {
                if is_blocked_component(name) {
                    return true;
                }
            }
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
    if let Some(ac) = secret_scanner() {
        if !ac.is_match(text) {
            return None;
        }
    }

    // Collect all replacement ranges in a single pass, then build output once.
    // This avoids O(n) replace_range calls that each shift the entire string.
    let mut replacements: Vec<(usize, usize)> = Vec::new();

    // Check for PEM private key blocks — uses pre-computed end markers.
    for &(marker, ref end_marker) in PEM_PAIRS.iter() {
        let mut search_from = 0;
        while let Some(start) = text[search_from..].find(marker) {
            let abs_start = search_from + start;
            if let Some(end) = text[abs_start..].find(end_marker.as_str()) {
                let block_end = abs_start + end + end_marker.len();
                replacements.push((abs_start, block_end));
                search_from = block_end;
            } else {
                break;
            }
        }
    }

    // Check for known secret prefixes
    for &(_label, prefix, min_len) in SECRET_PATTERNS {
        let mut search_from = 0;
        while let Some(pos) = text[search_from..].find(prefix) {
            let abs_pos = search_from + pos;
            let remaining = &text[abs_pos..];
            let token_end = remaining
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '`' || c == ',')
                .unwrap_or(remaining.len());

            if min_len == 0 || token_end >= min_len {
                replacements.push((abs_pos, abs_pos + token_end));
                search_from = abs_pos + token_end;
            } else {
                break;
            }
        }
    }

    if replacements.is_empty() {
        return None;
    }

    // Sort and merge overlapping ranges, then build output in one pass.
    replacements.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(replacements.len());
    for (start, end) in replacements {
        if let Some(last) = merged.last_mut() {
            if start < last.1 {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        } else {
            merged.push((start, end));
        }
    }

    // Build output: copy unchanged segments, insert REDACTED for replaced ranges.
    let mut output = String::with_capacity(text.len());
    let mut prev_end = 0;
    for (start, end) in &merged {
        output.push_str(&text[prev_end..*start]);
        output.push_str(REDACTED);
        prev_end = *end;
    }
    output.push_str(&text[prev_end..]);

    Some(output)
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

    /// A leaf with a benign extension (`.json`) must still be blocked when
    /// any intermediate path component is itself sensitive — otherwise paths
    /// like `vault/secret.pem/leaf.json` smuggle key material out under the
    /// guise of an unrelated file extension.
    #[test]
    fn blocks_intermediate_sensitive_components() {
        assert!(is_blocked_path(Path::new("vault/secret.pem/leaf.json")));
        assert!(is_blocked_path(Path::new("a/b/server.key/c.txt")));
        assert!(is_blocked_path(Path::new("project/.env/dump.json")));
        assert!(is_blocked_path(Path::new("project/.env.local/extra.txt")));
        assert!(is_blocked_path(Path::new("a/id_rsa/note.md")));

        // Negative controls: similarly-shaped paths with no sensitive component.
        assert!(!is_blocked_path(Path::new("vault/notes/leaf.json")));
        assert!(!is_blocked_path(Path::new("docs/key-rotation/runbook.md")));
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
