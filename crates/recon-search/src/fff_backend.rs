//! fff-grep based text search backend.
//!
//! Uses `fff_grep::Searcher` over memory-mapped file slices.
//! fff-grep 0.6 defines its own `Matcher` trait; we bridge to it via
//! a thin `FffMatcher` newtype that wraps `grep_regex::RegexMatcher`.

use crate::search_trait::{
    MultiSearchMeasured, TextHit, TextQuery, TextSearcher, MEASURED_BASELINE_CAP,
};
use crate::utils::regex_escape;
use aho_corasick::AhoCorasick;
use compact_str::CompactString;
use dashmap::DashMap;
use grep_regex::RegexMatcher;
use rayon::prelude::*;
use recon_core::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::instrument;

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

        // Evict ~25% of cache when at capacity to retain warm entries.
        if self.cache.len() >= MAX_CACHE_ENTRIES {
            let to_remove: Vec<PathBuf> = self
                .cache
                .iter()
                .take(MAX_CACHE_ENTRIES / 4)
                .map(|e| e.key().clone())
                .collect();
            for key in to_remove {
                self.cache.remove(&key);
            }
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
        let decoded = String::from_utf8_lossy(mat.bytes());
        self.hits.push(TextHit {
            path: self.path.to_path_buf(),
            line: mat.line_number().unwrap_or(0) as u32,
            col: None,
            line_text: CompactString::new(decoded.trim_end()),
        });
        Ok(true)
    }
}

/// Hard byte budget on the running-sum scan. Once we've accumulated
/// this many bytes of match-line content across every file in the
/// query scope, the `search_measured` outer loop stops scanning new
/// files and extrapolates the final number by `scope_total /
/// scope_scanned`. This is the worst-case latency mitigation called
/// out in the v0.4 measured-savings plan: pathological queries (a
/// regex matching every line of a giant repo) terminate in bounded
/// wall-time at the cost of a coarser baseline figure.
const MATCH_BYTE_BUDGET: u64 = 1 << 20; // 1 MiB

/// Sink for the measured-baseline scan. Differs from [`CollectSink`] in
/// two ways: it keeps accumulating `measured` past the hits cap (until
/// the per-call [`MEASURED_BASELINE_CAP`] is hit), and it stops the
/// scan only once neither more hits nor more counting are needed.
///
/// `measured`, `bytes_scanned`, and the hit vec are `&mut` shared
/// across files in a single `search_measured` call so the caps act
/// globally on the per-call totals, not per file.
struct MeasuredSink<'a> {
    path: &'a std::path::Path,
    hits: &'a mut Vec<TextHit>,
    max_hits: usize,
    measured: &'a mut u64,
    /// Running sum of match-line bytes seen this call. Distinct from
    /// `measured` (which is in tokens, post-divide-by-4 with the cap)
    /// — bytes give us a stable budget independent of token-cap
    /// saturation.
    bytes_scanned: &'a mut u64,
}

impl<'a> fff_grep::Sink for MeasuredSink<'a> {
    type Error = SinkErr;

    fn matched(
        &mut self,
        _searcher: &fff_grep::Searcher,
        mat: &fff_grep::SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        let bytes = mat.bytes();
        let n_bytes = bytes.len() as u64;
        *self.bytes_scanned = self.bytes_scanned.saturating_add(n_bytes);
        if *self.measured < MEASURED_BASELINE_CAP {
            let add = n_bytes.div_ceil(4);
            *self.measured = self.measured.saturating_add(add).min(MEASURED_BASELINE_CAP);
        }
        if self.hits.len() < self.max_hits {
            let decoded = String::from_utf8_lossy(bytes);
            self.hits.push(TextHit {
                path: self.path.to_path_buf(),
                line: mat.line_number().unwrap_or(0) as u32,
                col: None,
                line_text: CompactString::new(decoded.trim_end()),
            });
        }
        // Continue scanning while either side still needs work AND we're
        // under the byte budget. Once the hits vec is full, the measured
        // cap is saturated, OR we've exhausted the budget, no further
        // observation can change the result, so we stop.
        let need_more_hits = self.hits.len() < self.max_hits;
        let need_more_count = *self.measured < MEASURED_BASELINE_CAP;
        let under_byte_budget = *self.bytes_scanned < MATCH_BYTE_BUDGET;
        Ok((need_more_hits || need_more_count) && under_byte_budget)
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
fn extract_line_text(data: &[u8], start: usize, end: usize) -> CompactString {
    let trimmed = String::from_utf8_lossy(&data[start..end]);
    CompactString::new(trimmed.trim_end())
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
    #[instrument(
        skip(self, q),
        fields(
            pattern_len = q.pattern.len(),
            is_regex = q.is_regex,
            scope_files = q.scope.len(),
            max_results = q.max_results,
        ),
    )]
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

    #[instrument(
        skip(self, patterns, scope),
        fields(
            patterns = patterns.len(),
            scope_files = scope.len(),
            max_per_pattern,
        ),
    )]
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

    /// Override the default trait impl with a single non-truncating scan
    /// that returns both hits (capped at `q.max_results`) and the full
    /// measured baseline (capped at [`MEASURED_BASELINE_CAP`]).
    ///
    /// The default `search_measured` would only sum the truncated hits,
    /// undercounting on busy repos. We instead use [`MeasuredSink`]
    /// which keeps counting tokens past `q.max_results` until the
    /// per-call cap is hit, then stops the scanner.
    ///
    /// **Worst-case mitigation.** After [`MATCH_BYTE_BUDGET`] of match
    /// content has been scanned across the per-call totals, the loop
    /// stops touching new files and extrapolates the final figure by
    /// `scope_total / scope_scanned`. This caps the worst-case wall
    /// time on a regex that matches every line of a giant repo at the
    /// cost of a coarser baseline number — see the
    /// `bench_search_vs_measured` justification in
    /// `crates/recon-search/benches/search_bench.rs`.
    #[instrument(
        skip(self, q),
        fields(
            pattern_len = q.pattern.len(),
            is_regex = q.is_regex,
            scope_files = q.scope.len(),
            max_results = q.max_results,
        ),
    )]
    fn search_measured(&self, q: &TextQuery) -> Result<(Vec<TextHit>, u64), Error> {
        let matcher = build_matcher(&q.pattern, q.is_regex)?;
        let mut hits = Vec::with_capacity(q.max_results.min(64));
        let mut measured: u64 = 0;
        let mut bytes_scanned: u64 = 0;
        let searcher = fff_grep::SearcherBuilder::new().line_number(true).build();

        let scope_total = q.scope.len();
        let mut scope_scanned: usize = 0;
        for path in &q.scope {
            // Stop once neither side can still benefit from more bytes,
            // or once the byte budget for this call is exhausted.
            if hits.len() >= q.max_results && measured >= MEASURED_BASELINE_CAP {
                break;
            }
            if bytes_scanned >= MATCH_BYTE_BUDGET {
                break;
            }
            let Some(mmap) = self.get_mmap(path) else {
                scope_scanned += 1;
                continue;
            };
            scope_scanned += 1;
            let mut sink = MeasuredSink {
                path,
                hits: &mut hits,
                max_hits: q.max_results,
                measured: &mut measured,
                bytes_scanned: &mut bytes_scanned,
            };
            if let Err(e) = searcher.search_slice(&matcher, &mmap, &mut sink) {
                tracing::debug!(?path, "fff search_measured error: {e}");
            }
        }
        // If the byte budget terminated the scan early, extrapolate by
        // the unscanned-file ratio. Even when extrapolation overshoots
        // the per-call token cap, [`MEASURED_BASELINE_CAP`] still bounds
        // it on the way out.
        if bytes_scanned >= MATCH_BYTE_BUDGET && scope_scanned < scope_total && scope_scanned > 0 {
            let scaled =
                (measured as u128).saturating_mul(scope_total as u128) / scope_scanned as u128;
            measured = (scaled as u64).min(MEASURED_BASELINE_CAP);
        }
        Ok((hits, measured))
    }

    /// Override of [`TextSearcher::multi_search_measured`] that runs the
    /// existing aho-corasick parallel scan but tags each match with its
    /// byte length for a shared per-call accumulator. The default trait
    /// fallback would only count truncated hits — a regex with thousands
    /// of matches but `max_per_pattern = 20` would silently report
    /// `<= 20 × tokens-per-line` when the agent's grep would have seen
    /// the full count.
    ///
    /// The accumulator uses an `AtomicU64` because rayon distributes
    /// files across threads; sharing a single relaxed counter is
    /// faster than a per-thread bucket merge and the cap check is
    /// inherently approximate (we may overshoot by one match worth of
    /// tokens before the global value reaches the cap, which is fine).
    ///
    /// **Worst-case mitigation.** Before each file's scan, threads
    /// check the shared `bytes_scanned` counter; once it crosses
    /// [`MATCH_BYTE_BUDGET`], later files skip the inner scan and
    /// the final number is extrapolated by `scope_total /
    /// files_actually_scanned`. Mirrors the single-pattern path's
    /// strategy.
    #[instrument(
        skip(self, patterns, scope),
        fields(
            patterns = patterns.len(),
            scope_files = scope.len(),
            max_per_pattern,
        ),
    )]
    fn multi_search_measured(
        &self,
        patterns: &[&str],
        scope: &[PathBuf],
        max_per_pattern: usize,
    ) -> Result<MultiSearchMeasured, Error> {
        let non_empty: Vec<&str> = patterns.iter().copied().filter(|p| !p.is_empty()).collect();
        if non_empty.is_empty() {
            return Ok((
                patterns
                    .iter()
                    .map(|p| (p.to_string(), Vec::new()))
                    .collect(),
                0,
            ));
        }

        let ac = AhoCorasick::builder()
            .build(&non_empty)
            .map_err(|e| Error::Search(format!("aho-corasick build error: {e}")))?;

        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
        let measured = AtomicU64::new(0);
        let bytes_scanned = AtomicU64::new(0);
        let scope_scanned = AtomicUsize::new(0);
        let n_patterns = non_empty.len();
        let mut results: Vec<Vec<TextHit>> = scope
            .par_iter()
            .map(|path: &PathBuf| {
                let mut local_buckets: Vec<Vec<TextHit>> =
                    (0..n_patterns).map(|_| Vec::with_capacity(8)).collect();

                // Skip when another rayon worker has already burned
                // through the byte budget. The remaining files still
                // count toward the extrapolation denominator via
                // `scope_total`, so the final number reflects the full
                // scope.
                if bytes_scanned.load(Ordering::Relaxed) >= MATCH_BYTE_BUDGET {
                    return local_buckets;
                }

                let Some(mmap) = self.get_mmap(path) else {
                    return local_buckets;
                };
                scope_scanned.fetch_add(1, Ordering::Relaxed);

                let line_index = build_line_index(&mmap);
                let data_len = mmap.len();

                for mat in ac.find_iter(&*mmap) {
                    let pat_idx = mat.pattern().as_usize();
                    let start = mat.start();
                    let (line_num, line_start, line_end) =
                        resolve_line(&line_index, data_len, start);

                    let line_bytes = (line_end - line_start) as u64;
                    bytes_scanned.fetch_add(line_bytes, Ordering::Relaxed);

                    // Measured: every match contributes its line bytes,
                    // including matches the truncated bucket would drop.
                    if measured.load(Ordering::Relaxed) < MEASURED_BASELINE_CAP {
                        let add = line_bytes.div_ceil(4);
                        measured.fetch_add(add, Ordering::Relaxed);
                    }

                    if local_buckets[pat_idx].len() >= max_per_pattern {
                        continue;
                    }
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

        let scanned = scope_scanned.load(Ordering::Relaxed);
        let mut total = measured.load(Ordering::Relaxed);
        if bytes_scanned.load(Ordering::Relaxed) >= MATCH_BYTE_BUDGET
            && scanned < scope.len()
            && scanned > 0
        {
            let scaled = (total as u128).saturating_mul(scope.len() as u128) / scanned as u128;
            total = scaled as u64;
        }
        let total = total.min(MEASURED_BASELINE_CAP);
        Ok((final_results, total))
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
