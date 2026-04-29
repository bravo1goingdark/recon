//! `TextSearcher` trait — backend-agnostic text search interface.
//!
//! Implementations: [`GrepBackend`](crate::text::GrepBackend) (ripgrep crates),
//! [`FffBackend`](crate::fff_backend::FffBackend) (fff-grep).

use compact_str::CompactString;
use recon_core::error::Error;
use std::path::PathBuf;

/// Return type of [`TextSearcher::multi_search_measured`]: the
/// per-pattern truncated hit buckets paired with a single global
/// measured-baseline accumulator (in tokens, capped at
/// [`MEASURED_BASELINE_CAP`]).
pub type MultiSearchMeasured = (Vec<(String, Vec<TextHit>)>, u64);

/// Maximum tokens accumulated for the measured baseline, per call.
///
/// Bounds the worst case where a regex matches every line in a giant
/// repo. The "Read+grep equivalent" the agent would have paid for is
/// already truncated by their own context window long before this
/// figure, so anything past the cap is noise. Used by
/// [`TextSearcher::search_measured`] /
/// [`TextSearcher::multi_search_measured`] implementations.
pub const MEASURED_BASELINE_CAP: u64 = 1_000_000;

/// A query for text search.
#[derive(Debug, Clone)]
pub struct TextQuery {
    /// Pattern to search for.
    pub pattern: String,
    /// Whether to interpret `pattern` as a regex.
    pub is_regex: bool,
    /// Maximum number of results to return.
    pub max_results: usize,
    /// Files to search within.
    pub scope: Vec<PathBuf>,
}

/// A single text search hit.
#[derive(Debug, Clone)]
pub struct TextHit {
    /// Path to the file containing the hit.
    pub path: PathBuf,
    /// 1-based line number.
    pub line: u32,
    /// 1-based column, if available.
    pub col: Option<u32>,
    /// The matched line text. `CompactString` inlines short lines on the
    /// stack (≤24 bytes on 64-bit) and serializes identically to `String`.
    pub line_text: CompactString,
}

/// Backend-agnostic text search interface.
///
/// Implementations must be thread-safe for concurrent MCP tool calls.
pub trait TextSearcher: Send + Sync {
    /// Search for a single pattern within the given scope.
    fn search(&self, q: &TextQuery) -> Result<Vec<TextHit>, Error>;

    /// Search for multiple patterns simultaneously.
    ///
    /// Returns `(pattern, hits)` pairs. Implementations may use aho-corasick
    /// for single-pass multi-pattern, or iterate patterns individually.
    fn multi_search(
        &self,
        patterns: &[&str],
        scope: &[PathBuf],
        max_per_pattern: usize,
    ) -> Result<Vec<(String, Vec<TextHit>)>, Error>;

    /// Notify the backend that files have changed (invalidate caches).
    fn refresh(&self, changed_paths: &[PathBuf]) -> Result<(), Error>;

    /// Search and return both the truncated hits and the total tokens
    /// an unbounded grep alternative would have emitted (the *measured
    /// baseline*).
    ///
    /// The returned baseline is summed across **all** matches the
    /// scanner would have produced with no `max_results` cap, capped
    /// at [`MEASURED_BASELINE_CAP`]. Implementations that short-circuit
    /// `search` once the cap is reached must override this to do a
    /// distinct, cap-free scan; the default fallback only sums the
    /// truncated `search` output and undercounts on busy repos.
    fn search_measured(&self, q: &TextQuery) -> Result<(Vec<TextHit>, u64), Error> {
        let hits = self.search(q)?;
        let mut total: u64 = 0;
        for h in &hits {
            total = total.saturating_add(h.line_text.len().div_ceil(4) as u64);
            if total >= MEASURED_BASELINE_CAP {
                total = MEASURED_BASELINE_CAP;
                break;
            }
        }
        Ok((hits, total))
    }

    /// Multi-pattern variant of [`Self::search_measured`]. Returns the
    /// per-pattern truncated hit lists and a single shared accumulator
    /// covering every match across every pattern, capped at
    /// [`MEASURED_BASELINE_CAP`]. The shared cap mirrors how an agent
    /// running N greps would budget against a single context window.
    fn multi_search_measured(
        &self,
        patterns: &[&str],
        scope: &[PathBuf],
        max_per_pattern: usize,
    ) -> Result<MultiSearchMeasured, Error> {
        let results = self.multi_search(patterns, scope, max_per_pattern)?;
        let mut total: u64 = 0;
        'outer: for (_, hits) in &results {
            for h in hits {
                total = total.saturating_add(h.line_text.len().div_ceil(4) as u64);
                if total >= MEASURED_BASELINE_CAP {
                    total = MEASURED_BASELINE_CAP;
                    break 'outer;
                }
            }
        }
        Ok((results, total))
    }
}
