//! fff-grep based text search backend.
//!
//! Uses `fff_grep::Searcher` over memory-mapped file slices.
//! fff-grep 0.6 defines its own `Matcher` trait; we bridge to it via
//! a thin `FffMatcher` newtype that wraps `grep_regex::RegexMatcher`.

use crate::search_trait::{TextHit, TextQuery, TextSearcher};
use crate::utils::regex_escape;
use grep_regex::RegexMatcher;
use recon_core::error::Error;
use std::path::PathBuf;

/// fff-grep backed text search.
///
/// Memory-maps each file and delegates search to `fff_grep::Searcher::search_slice`.
pub struct FffBackend;

impl FffBackend {
    /// Create a new `FffBackend`.
    pub fn new() -> Self {
        Self
    }
}

impl Default for FffBackend {
    fn default() -> Self {
        Self::new()
    }
}

// ── Matcher bridge ─────────────────────────────────────────────────────────────

/// Adapts `grep_regex::RegexMatcher` to implement `fff_grep::Matcher`.
///
/// fff-grep 0.6 defines its own `Matcher` trait (simpler than the ripgrep
/// `grep-matcher` one), so we need this newtype to satisfy the trait bound.
struct FffMatcher(RegexMatcher);

impl fff_grep::Matcher for FffMatcher {
    type Error = fff_grep::NoError;

    fn find_at(&self, haystack: &[u8], at: usize) -> Result<Option<fff_grep::Match>, Self::Error> {
        use grep_matcher::Matcher;
        Ok(self
            .0
            .find_at(haystack, at)
            // RegexMatcher never returns Err — unwrap_or is safe
            .unwrap_or(None)
            .map(|m| fff_grep::Match::new(m.start(), m.end())))
    }
}

// ── Sink ───────────────────────────────────────────────────────────────────────

/// Newtype error for the fff_grep Sink (orphan rule prevents impl on std types).
#[derive(Debug)]
struct SinkErr(String);

impl std::fmt::Display for SinkErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SinkErr {}

impl fff_grep::SinkError for SinkErr {
    fn error_message<T: std::fmt::Display>(message: T) -> Self {
        SinkErr(message.to_string())
    }
}

/// A sink that collects matches into a `Vec<TextHit>`.
struct CollectSink<'a> {
    path: &'a std::path::Path,
    hits: &'a mut Vec<TextHit>,
    max: usize,
}

impl<'a> fff_grep::Sink for CollectSink<'a> {
    type Error = SinkErr;

    fn matched(
        &mut self,
        _searcher: &fff_grep::Searcher,
        mat: &fff_grep::SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self.hits.len() >= self.max {
            return Ok(false);
        }
        // Use lossy decoding so non-UTF-8 files still produce visible hit lines
        // rather than silently empty strings.
        let line_text = String::from_utf8_lossy(mat.bytes()).trim_end().to_string();
        self.hits.push(TextHit {
            path: self.path.to_path_buf(),
            line: mat.line_number().unwrap_or(0) as u32,
            col: None,
            line_text,
        });
        Ok(true)
    }
}

// ── Core search logic ──────────────────────────────────────────────────────────

/// Search a single memory-mapped file slice with fff-grep.
fn search_slice_in_file(
    matcher: &FffMatcher,
    path: &std::path::Path,
    hits: &mut Vec<TextHit>,
    max: usize,
) -> Result<(), Error> {
    let file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    if meta.len() == 0 {
        return Ok(());
    }

    // SAFETY: file is open and non-empty; we only read during the search.
    let mmap = unsafe { memmap2::Mmap::map(&file)? };

    let searcher = fff_grep::SearcherBuilder::new().line_number(true).build();

    let mut sink = CollectSink { path, hits, max };
    searcher
        .search_slice(matcher, &mmap, &mut sink)
        .map_err(|e| Error::Search(format!("fff-grep: {e}")))?;

    Ok(())
}

fn build_matcher(pattern: &str, is_regex: bool) -> Result<FffMatcher, Error> {
    let pat = if is_regex {
        pattern.to_string()
    } else {
        regex_escape(pattern)
    };
    RegexMatcher::new(&pat)
        .map(FffMatcher)
        .map_err(|e| Error::Search(format!("invalid pattern: {e}")))
}

// ── TextSearcher impl ──────────────────────────────────────────────────────────

impl TextSearcher for FffBackend {
    fn search(&self, q: &TextQuery) -> Result<Vec<TextHit>, Error> {
        let matcher = build_matcher(&q.pattern, q.is_regex)?;
        let mut hits = Vec::with_capacity(q.max_results.min(64));

        for path in &q.scope {
            if hits.len() >= q.max_results {
                break;
            }
            if let Err(e) = search_slice_in_file(&matcher, path, &mut hits, q.max_results) {
                tracing::debug!(?path, "fff search error: {e}");
            }
        }
        Ok(hits)
    }

    fn multi_search(
        &self,
        patterns: &[&str],
        scope: &[PathBuf],
        max_per_pattern: usize,
    ) -> Result<Vec<(String, Vec<TextHit>)>, Error> {
        let mut results = Vec::with_capacity(patterns.len());
        for &pat in patterns {
            let q = TextQuery {
                pattern: pat.to_string(),
                is_regex: false,
                max_results: max_per_pattern,
                scope: scope.to_vec(),
            };
            let hits = self.search(&q)?;
            results.push((pat.to_string(), hits));
        }
        Ok(results)
    }

    fn refresh(&self, _changed_paths: &[PathBuf]) -> Result<(), Error> {
        // FffBackend mmaps files on each query — nothing to invalidate.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn fff_search_exact() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn hello() {}\nfn world() {}\nfn hello_world() {}").unwrap();

        let backend = FffBackend::new();
        let q = TextQuery {
            pattern: "hello".into(),
            is_regex: false,
            max_results: 100,
            scope: vec![file],
        };
        let hits = backend.search(&q).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].line_text.contains("hello"));
    }

    #[test]
    fn fff_search_regex() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn foo_bar() {}\nfn baz_qux() {}\nfn foo_qux() {}").unwrap();

        let backend = FffBackend::new();
        let q = TextQuery {
            pattern: "foo_\\w+".into(),
            is_regex: true,
            max_results: 100,
            scope: vec![file],
        };
        let hits = backend.search(&q).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn fff_search_max_results() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        let content: String = (0..100).map(|i| format!("fn func_{i}() {{}}\n")).collect();
        std::fs::write(&file, content).unwrap();

        let backend = FffBackend::new();
        let q = TextQuery {
            pattern: "fn func_".into(),
            is_regex: false,
            max_results: 5,
            scope: vec![file],
        };
        let hits = backend.search(&q).unwrap();
        assert_eq!(hits.len(), 5);
    }

    #[test]
    fn fff_multi_search() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn foo() {}\nfn bar() {}\nfn baz() {}").unwrap();

        let backend = FffBackend::new();
        let results = backend.multi_search(&["foo", "bar"], &[file], 10).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].1.len(), 1);
        assert_eq!(results[1].1.len(), 1);
    }

    #[test]
    fn fff_search_empty_file_ok() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("empty.rs");
        std::fs::write(&file, b"").unwrap();
        let backend = FffBackend::new();
        let q = TextQuery {
            pattern: "anything".into(),
            is_regex: false,
            max_results: 10,
            scope: vec![file],
        };
        let hits = backend.search(&q).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn fff_search_no_matches() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("no_match.rs");
        std::fs::write(&file, "fn hello() {}").unwrap();
        let backend = FffBackend::new();
        let q = TextQuery {
            pattern: "xyz_not_present".into(),
            is_regex: false,
            max_results: 10,
            scope: vec![file],
        };
        let hits = backend.search(&q).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn fff_search_special_chars_escaped() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("special.rs");
        std::fs::write(&file, "let x = vec![1, 2, 3];").unwrap();
        let backend = FffBackend::new();
        let q = TextQuery {
            pattern: "vec![1".into(), // contains regex special chars
            is_regex: false,
            max_results: 10,
            scope: vec![file],
        };
        let hits = backend.search(&q).unwrap();
        assert_eq!(hits.len(), 1, "should find literal vec![1");
    }

    #[test]
    fn fff_search_nonexistent_file_skipped() {
        let backend = FffBackend::new();
        let q = TextQuery {
            pattern: "anything".into(),
            is_regex: false,
            max_results: 10,
            scope: vec![PathBuf::from("/nonexistent/file.rs")],
        };
        // Should not panic — error is logged and skipped.
        let hits = backend.search(&q).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn fff_multi_search_empty_patterns() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn foo() {}").unwrap();
        let backend = FffBackend::new();
        let results = backend.multi_search(&[], &[file], 10).unwrap();
        assert!(results.is_empty());
    }
}
