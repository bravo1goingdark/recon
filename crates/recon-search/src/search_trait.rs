//! `TextSearcher` trait — backend-agnostic text search interface.
//!
//! Implementations: [`GrepBackend`](crate::text::GrepBackend) (ripgrep crates),
//! [`FffBackend`](crate::fff_backend::FffBackend) (fff-grep).

use compact_str::CompactString;
use recon_core::error::Error;
use std::path::PathBuf;

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
}
