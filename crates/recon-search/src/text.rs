//! Text search using ripgrep's grep-* crates, behind the [`TextSearcher`] trait.

use crate::search_trait::{TextHit, TextQuery, TextSearcher};
use crate::utils::regex_escape;
use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::Searcher;
use recon_core::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Ripgrep-based text search backend.
///
/// Reads files from disk on every query — no cache to invalidate.
pub struct GrepBackend;

impl GrepBackend {
    /// Create a new `GrepBackend`.
    pub fn new() -> Self {
        Self
    }
}

impl Default for GrepBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TextSearcher for GrepBackend {
    fn search(&self, q: &TextQuery) -> Result<Vec<TextHit>, Error> {
        search_files(&q.pattern, &q.scope, q.is_regex, q.max_results)
    }

    fn multi_search(
        &self,
        patterns: &[&str],
        scope: &[PathBuf],
        max_per_pattern: usize,
    ) -> Result<Vec<(String, Vec<TextHit>)>, Error> {
        let mut results = Vec::with_capacity(patterns.len());
        for &pat in patterns {
            let hits = search_files(pat, scope, false, max_per_pattern)?;
            results.push((pat.to_string(), hits));
        }
        Ok(results)
    }

    fn refresh(&self, _changed_paths: &[PathBuf]) -> Result<(), Error> {
        // GrepBackend reads from disk on every query — nothing to invalidate.
        Ok(())
    }
}

// ---- convenience free functions (backward compat) ----

/// Search for a pattern across files using the ripgrep backend.
pub fn search_files(
    pattern: &str,
    paths: &[PathBuf],
    is_regex: bool,
    max_results: usize,
) -> Result<Vec<TextHit>, Error> {
    let matcher = if is_regex {
        RegexMatcher::new(pattern)
    } else {
        RegexMatcher::new(&regex_escape(pattern))
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
pub fn search_file(pattern: &str, path: &Path, is_regex: bool) -> Result<Vec<TextHit>, Error> {
    search_files(pattern, &[path.to_path_buf()], is_regex, 1000)
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
        assert_eq!(hits.len(), 2);
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

    #[test]
    fn grep_backend_via_trait() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn alpha() {}\nfn beta() {}\nfn alpha_beta() {}").unwrap();

        let backend = GrepBackend::new();
        let q = TextQuery {
            pattern: "alpha".into(),
            is_regex: false,
            max_results: 100,
            scope: vec![file],
        };
        let hits = backend.search(&q).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn grep_backend_multi_search() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn foo() {}\nfn bar() {}\nfn baz() {}").unwrap();

        let backend = GrepBackend::new();
        let results = backend.multi_search(&["foo", "bar"], &[file], 10).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "foo");
        assert_eq!(results[0].1.len(), 1);
        assert_eq!(results[1].0, "bar");
        assert_eq!(results[1].1.len(), 1);
    }
}
