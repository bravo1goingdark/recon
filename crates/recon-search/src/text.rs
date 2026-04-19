//! Text search using ripgrep's grep-* crates.

use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::Searcher;
use recon_core::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A single text search hit.
#[derive(Debug, Clone)]
pub struct TextHit {
    pub path: PathBuf,
    pub line: u32,
    pub col: Option<u32>,
    pub line_text: String,
}

/// Search for a pattern across files.
pub fn search_files(
    pattern: &str,
    paths: &[PathBuf],
    is_regex: bool,
    max_results: usize,
) -> Result<Vec<TextHit>, Error> {
    let matcher = if is_regex {
        RegexMatcher::new(pattern)
    } else {
        RegexMatcher::new(&regex::escape(pattern))
    }
    .map_err(|e| Error::Search(format!("invalid pattern: {e}")))?;

    let mut hits = Vec::with_capacity(max_results.min(64));
    let mut searcher = Searcher::new();

    for path in paths {
        if hits.len() >= max_results {
            break;
        }
        let shared_path: Arc<PathBuf> = Arc::new(path.clone());
        let result = searcher.search_path(
            &matcher,
            path,
            UTF8(|line_num, line_text| {
                if hits.len() >= max_results {
                    return Ok(false);
                }
                let col = matcher
                    .find(line_text.as_bytes())
                    .ok()
                    .flatten()
                    .map(|m| m.start() as u32 + 1);

                hits.push(TextHit {
                    path: (*shared_path).clone(),
                    line: line_num as u32,
                    col,
                    line_text: line_text.trim_end().to_string(),
                });
                Ok(true)
            }),
        );

        if let Err(e) = result {
            tracing::debug!(?path, "search error: {e}");
        }
    }

    Ok(hits)
}

/// Search for a pattern in a single file.
pub fn search_file(
    pattern: &str,
    path: &Path,
    is_regex: bool,
) -> Result<Vec<TextHit>, Error> {
    search_files(pattern, &[path.to_path_buf()], is_regex, 1000)
}

fn regex_escape(pattern: &str) -> String {
    let mut escaped = String::with_capacity(pattern.len());
    for c in pattern.chars() {
        if "\\.*+?()[]{}|^$".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

mod regex {
    pub fn escape(pattern: &str) -> String {
        super::regex_escape(pattern)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn search_exact() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn hello() {}\nfn world() {}\nfn hello_world() {}").unwrap();

        let hits = search_files("hello", &[file], false, 100).unwrap();
        assert_eq!(hits.len(), 2); // "hello" and "hello_world"
        assert!(hits[0].line_text.contains("hello"));
    }

    #[test]
    fn search_regex() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn foo_bar() {}\nfn baz_qux() {}\nfn foo_qux() {}").unwrap();

        let hits = search_files("foo_\\w+", &[file], true, 100).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn search_max_results() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        let content: String = (0..100).map(|i| format!("fn func_{i}() {{}}\n")).collect();
        std::fs::write(&file, content).unwrap();

        let hits = search_files("fn func_", &[file], false, 5).unwrap();
        assert_eq!(hits.len(), 5);
    }
}
