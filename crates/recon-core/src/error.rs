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

/// Stable numeric codes returned on tool-error responses.
///
/// Values live in the JSON-RPC "application" range `-32000..=-32099`
/// so they cannot collide with the framework's reserved codes. Codes
/// are part of the public tool contract — once assigned, never reuse
/// a value for a different meaning.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconErrorCode {
    /// Tool arguments failed JSON deserialization or parameter validation.
    InvalidParams = -32001,
    /// Requested symbol / file / tool does not exist.
    NotFound = -32002,
    /// Request exceeded the server-side deadline.
    Timeout = -32003,
    /// SQLite / vector-store / Tantivy storage-layer failure.
    Storage = -32004,
    /// Tree-sitter or language-grammar failure.
    Parse = -32005,
    /// Search layer (Tantivy, fff-grep) failure.
    Search = -32006,
    /// Resolved path escapes the repo root — possible traversal attempt.
    PathTraversal = -32007,
    /// Request targets a path blocked by the redaction list.
    PermissionDenied = -32008,
    /// Target file exceeds the read size cap.
    FileTooLarge = -32009,
    /// I/O failure (file system, network).
    Io = -32010,
    /// Subscription has expired or no valid license is cached. Agent should
    /// surface "run `recon login <key>` to renew" to the user.
    LicenseExpired = -32011,
    /// Server-side budget exhausted (graph traversal visit cap, response
    /// truncation cap). The query is well-formed but the answer would be
    /// too large; agent should narrow the input.
    ResourceExhausted = -32012,
    /// Anything else — internal invariant, bug, unexpected state.
    Internal = -32099,
}

impl ReconErrorCode {
    /// The numeric wire code.
    #[inline]
    pub fn code(self) -> i32 {
        self as i32
    }

    /// Stable kebab-case identifier for matching in client code.
    pub fn kind(self) -> &'static str {
        match self {
            ReconErrorCode::InvalidParams => "invalid_params",
            ReconErrorCode::NotFound => "not_found",
            ReconErrorCode::Timeout => "timeout",
            ReconErrorCode::Storage => "storage",
            ReconErrorCode::Parse => "parse",
            ReconErrorCode::Search => "search",
            ReconErrorCode::PathTraversal => "path_traversal",
            ReconErrorCode::PermissionDenied => "permission_denied",
            ReconErrorCode::FileTooLarge => "file_too_large",
            ReconErrorCode::Io => "io",
            ReconErrorCode::LicenseExpired => "license_expired",
            ReconErrorCode::ResourceExhausted => "resource_exhausted",
            ReconErrorCode::Internal => "internal",
        }
    }
}

impl Error {
    /// Map the internal error variant to a stable client-facing code.
    pub fn rpc_code(&self) -> ReconErrorCode {
        match self {
            Error::Io(_) => ReconErrorCode::Io,
            Error::Parse(_) => ReconErrorCode::Parse,
            Error::Storage(_) => ReconErrorCode::Storage,
            Error::Search(_) => ReconErrorCode::Search,
            Error::Protocol(_) => ReconErrorCode::Internal,
            Error::PathTraversal(_) => ReconErrorCode::PathTraversal,
            Error::Config(_) => ReconErrorCode::Internal,
        }
    }
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
