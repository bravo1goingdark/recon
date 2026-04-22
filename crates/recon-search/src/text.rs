//! Text search using ripgrep's grep-* crates, behind the [`TextSearcher`] trait.

use crate::search_trait::{TextHit, TextQuery, TextSearcher};
use crate::utils::regex_escape;
use aho_corasick::AhoCorasick;
use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::Searcher;
use rayon::prelude::*;
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

// ── Helper functions for aho-corasick line extraction ──────────────────────────

/// Build a sorted list of newline byte offsets for binary-search line resolution.
fn build_line_index(data: &[u8]) -> Vec<usize> {
    let mut newlines = Vec::with_capacity(data.len() / 64);
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            newlines.push(i);
        }
    }
    newlines
}

/// Resolve byte offset to (line_number, line_start, line_end) using binary search.
#[inline]
fn resolve_line(line_index: &[usize], data_len: usize, offset: usize) -> (u32, usize, usize) {
    let idx = line_index.partition_point(|&nl| nl < offset);
    let line_num = (idx + 1) as u32;
    let line_start = if idx == 0 { 0 } else { line_index[idx - 1] + 1 };
    let line_end = if idx < line_index.len() {
        line_index[idx]
    } else {
        data_len
    };
    (line_num, line_start, line_end)
}

/// Extract line text from byte range, decoded lossily.
#[inline]
fn extract_line_text(data: &[u8], start: usize, end: usize) -> String {
    String::from_utf8_lossy(&data[start..end])
        .trim_end()
        .to_string()
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
        let non_empty: Vec<&str> = patterns.iter().copied().filter(|p| !p.is_empty()).collect();
        if non_empty.is_empty() {
            return Ok(patterns
                .iter()
                .map(|p| (p.to_string(), Vec::new()))
                .collect());
        }

        let ac = AhoCorasick::builder()
            .build(&non_empty)
            .map_err(|e| Error::Search(format!("aho-corasick build error: {e}")))?;

        // Parallel scan: partition files across rayon threads, merge buckets.
        let n_patterns = non_empty.len();
        let mut results: Vec<Vec<TextHit>> = scope
            .par_iter()
            .map(|path: &PathBuf| {
                let mut local_buckets: Vec<Vec<TextHit>> =
                    (0..n_patterns).map(|_| Vec::with_capacity(8)).collect();

                let Ok(data) = std::fs::read(path) else {
                    return local_buckets;
                };
                if data.is_empty() {
                    return local_buckets;
                }

                let line_index = build_line_index(&data);
                let data_len = data.len();

                for mat in ac.find_iter(&data) {
                    let pat_idx = mat.pattern().as_usize();
                    if local_buckets[pat_idx].len() >= max_per_pattern {
                        continue;
                    }
                    let start = mat.start();
                    let (line_num, line_start, line_end) =
                        resolve_line(&line_index, data_len, start);
                    let line_text = extract_line_text(&data, line_start, line_end);
                    let col = (start - line_start) as u32 + 1;
                    local_buckets[pat_idx].push(TextHit {
                        path: path.clone(),
                        line: line_num,
                        col: Some(col),
                        line_text,
                    });
                }
                local_buckets
            })
            .reduce(
                || (0..n_patterns).map(|_| Vec::new()).collect(),
                |mut acc, buckets| {
                    for (i, b) in buckets.into_iter().enumerate() {
                        acc[i].extend(b);
                    }
                    acc
                },
            );

        let mut final_results = Vec::with_capacity(patterns.len());
        let mut non_empty_idx = 0;
        for &pat in patterns {
            if pat.is_empty() {
                final_results.push((pat.to_string(), Vec::new()));
            } else {
                let mut hits = std::mem::take(&mut results[non_empty_idx]);
                hits.truncate(max_per_pattern);
                final_results.push((pat.to_string(), hits));
                non_empty_idx += 1;
            }
        }
        Ok(final_results)
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
