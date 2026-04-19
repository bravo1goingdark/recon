//! Error types for recon.

use thiserror::Error;

/// Top-level error type used across recon crates.
#[derive(Debug, Error)]
pub enum Error {
    /// I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Tree-sitter parse failure.
    #[error("parse error: {0}")]
    Parse(String),

    /// Storage/database failure.
    #[error("storage error: {0}")]
    Storage(String),

    /// Search engine failure.
    #[error("search error: {0}")]
    Search(String),

    /// MCP protocol error.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Path escapes the repo root.
    #[error("path traversal: {0}")]
    PathTraversal(String),

    /// Configuration error.
    #[error("config error: {0}")]
    Config(String),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        let e = Error::Parse("unexpected token".into());
        assert_eq!(e.to_string(), "parse error: unexpected token");
    }

    #[test]
    fn io_error_converts() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let e: Error = io_err.into();
        assert!(e.to_string().contains("gone"));
    }
}
