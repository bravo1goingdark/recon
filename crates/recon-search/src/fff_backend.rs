//! fff-grep based text search backend.
//!
//! Uses `fff_grep::Searcher` over memory-mapped file slices.
//! fff-grep 0.6 defines its own `Matcher` trait; we bridge to it via
//! a thin `FffMatcher` newtype that wraps `grep_regex::RegexMatcher`.

use crate::search_trait::{TextHit, TextQuery, TextSearcher};
use crate::utils::regex_escape;
use aho_corasick::AhoCorasick;
use dashmap::DashMap;
use grep_regex::RegexMatcher;
use rayon::prelude::*;
use recon_core::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

/// Maximum number of entries in the mmap cache.
const MAX_CACHE_ENTRIES: usize = 1024;

/// fff-grep backed text search.
///
/// Memory-maps each file and delegates search to `fff_grep::Searcher::search_slice`.
/// Uses `DashMap` for concurrent lock-free reads and fine-grained write locking —
/// no full-map clone on cache miss (unlike the previous ArcSwap approach).
pub struct FffBackend {
    /// Concurrent mmap cache — reads are lock-free, writes lock only one shard.
    cache: DashMap<PathBuf, Arc<memmap2::Mmap>>,
}

impl FffBackend {
    /// Create a new `FffBackend`.
    pub fn new() -> Self {
        Self {
            cache: DashMap::with_capacity(MAX_CACHE_ENTRIES),
        }
    }

    /// Look up or create an mmap for the given path.
    fn get_mmap(&self, path: &std::path::Path) -> Option<Arc<memmap2::Mmap>> {
        // Fast path: lock-free read from DashMap.
        if let Some(mmap) = self.cache.get(path) {
            return Some(Arc::clone(&mmap));
        }

        // Not in cache — mmap the file.
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(?path, "get_mmap: cannot open file: {e}");
                return None;
            }
        };
        let meta = match file.metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(?path, "get_mmap: cannot stat file: {e}");
                return None;
            }
        };
        if meta.len() == 0 {
            return None;
        }

        // SAFETY: file is open and non-empty; we only read during the search.
        let mmap = match unsafe { memmap2::Mmap::map(&file) } {
            Ok(m) => Arc::new(m),
            Err(e) => {
                tracing::debug!(?path, "get_mmap: cannot mmap file: {e}");
                return None;
            }
        };

        // Evict if at capacity — DashMap handles concurrent insertion safely.
        if self.cache.len() >= MAX_CACHE_ENTRIES {
            self.cache.clear();
        }

        // Double-check: another thread may have inserted while we were mmap'ing.
        // DashMap::entry avoids the race entirely.
        use dashmap::mapref::entry::Entry;
        match self.cache.entry(path.to_path_buf()) {
            Entry::Occupied(e) => Some(Arc::clone(e.get())),
            Entry::Vacant(e) => {
                e.insert(Arc::clone(&mmap));
                Some(mmap)
            }
        }
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

// ── Helper functions for aho-corasick line extraction ──────────────────────────

/// Build a sorted list of newline byte offsets for binary-search line resolution.
/// O(M) one-time cost per file, then O(log M) per match.
fn build_line_index(data: &[u8]) -> Vec<usize> {
    let mut newlines = Vec::with_capacity(data.len() / 64); // heuristic: ~1 line per 64 bytes
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            newlines.push(i);
        }
    }
    newlines
}

/// Resolve byte offset to (line_number, line_start, line_end) using binary search.
/// line_number is 1-based.
#[inline]
fn resolve_line(line_index: &[usize], data_len: usize, offset: usize) -> (u32, usize, usize) {
    // Binary search: find first newline >= offset
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

// ── Core search logic ──────────────────────────────────────────────────────────

/// Search a single memory-mapped file slice with fff-grep.
fn search_slice_with_mmap(
    matcher: &FffMatcher,
    mmap: &memmap2::Mmap,
    path: &std::path::Path,
    hits: &mut Vec<TextHit>,
    max: usize,
) -> Result<(), Error> {
    let searcher = fff_grep::SearcherBuilder::new().line_number(true).build();

    let mut sink = CollectSink { path, hits, max };
    searcher
        .search_slice(matcher, mmap, &mut sink)
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
            let Some(mmap) = self.get_mmap(path) else {
                tracing::debug!(?path, "fff search: no mmap available");
                continue;
            };
            if let Err(e) = search_slice_with_mmap(&matcher, &mmap, path, &mut hits, q.max_results)
            {
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
        // Filter out empty patterns — aho-corasick rejects them.
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

        // Parallel scan across all files — rayon distributes work across CPU cores.
        // Each thread builds local buckets, then we merge them lock-free.
        let n_patterns = non_empty.len();
        let mut results: Vec<Vec<TextHit>> = scope
            .par_iter()
            .map(|path: &PathBuf| {
                let mut local_buckets: Vec<Vec<TextHit>> =
                    (0..n_patterns).map(|_| Vec::with_capacity(8)).collect();

                let Some(mmap) = self.get_mmap(path) else {
                    return local_buckets;
                };

                // Build newline index once per file: O(M)
                let line_index = build_line_index(&mmap);
                let data_len = mmap.len();

                // Single-pass aho-corasick scan: O(N log M) for N matches
                for mat in ac.find_iter(&*mmap) {
                    let pat_idx = mat.pattern().as_usize();
                    if local_buckets[pat_idx].len() >= max_per_pattern {
                        continue;
                    }
                    let start = mat.start();
                    let (line_num, line_start, line_end) =
                        resolve_line(&line_index, data_len, start);
                    let line_text = extract_line_text(&mmap, line_start, line_end);
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

        // Build results preserving original pattern order.
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

    fn refresh(&self, changed_paths: &[PathBuf]) -> Result<(), Error> {
        if self.cache.is_empty() {
            return Ok(());
        }

        // If changed paths are a significant fraction of cache, clear entirely.
        if changed_paths.len() >= self.cache.len() / 2 {
            self.cache.clear();
        } else {
            for path in changed_paths {
                self.cache.remove(path);
            }
        }
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

    #[test]
    fn fff_refresh_invalidates_cache() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn hello() {}").unwrap();

        let backend = FffBackend::new();
        let q = TextQuery {
            pattern: "fn ".into(),
            is_regex: false,
            max_results: 10,
            scope: vec![file.clone()],
        };
        let hits = backend.search(&q).unwrap();
        assert_eq!(hits.len(), 1);

        // Refresh should invalidate the cache for this file.
        backend.refresh(std::slice::from_ref(&file)).unwrap();

        // Modify the file on disk.
        std::fs::write(&file, "fn hello() {}\nfn world() {}").unwrap();

        // Next search should pick up the new content.
        let hits = backend.search(&q).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn fff_refresh_clears_on_many_changes() {
        let dir = tempdir().unwrap();
        let files: Vec<_> = (0..5)
            .map(|i| {
                let f = dir.path().join(format!("file_{i}.rs"));
                std::fs::write(&f, format!("fn func_{i}() {{}}")).unwrap();
                f
            })
            .collect();

        let backend = FffBackend::new();

        // Search all files to populate cache.
        for f in &files {
            let q = TextQuery {
                pattern: format!("func_{}", files.iter().position(|x| x == f).unwrap()),
                is_regex: false,
                max_results: 10,
                scope: vec![f.clone()],
            };
            let hits = backend.search(&q).unwrap();
            assert_eq!(hits.len(), 1);
        }

        // Refresh with >= half the cached entries — should clear entirely.
        let changed: Vec<_> = files.iter().take(3).cloned().collect();
        backend.refresh(&changed).unwrap();

        // Cache should now be empty.
        assert!(backend.cache.is_empty());
    }
}
