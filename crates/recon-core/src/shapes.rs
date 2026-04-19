//! The five canonical output shapes for MCP tool responses.

use crate::symbol::SymbolKind;
use serde::Serialize;
use std::path::PathBuf;

/// Every tool returns exactly one of these shapes.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "shape")]
pub enum ToolOutput {
    /// One line per top-level symbol.
    Outline(OutlineView),
    /// Signatures + docstrings, bodies elided.
    Skeleton(SkeletonView),
    /// Full body of one symbol plus context.
    SymbolCard(SymbolCardView),
    /// Count + top-k call sites.
    ReferenceDigest(RefDigestView),
    /// Diagnostic messages in `file:line:col: msg` form.
    Diagnostics(DiagView),
}

/// Outline view — one line per top-level symbol.
#[derive(Debug, Clone, Serialize)]
pub struct OutlineView {
    /// File path.
    pub path: PathBuf,
    /// Top-level symbol entries.
    pub entries: Vec<OutlineEntry>,
}

/// A single entry in an outline.
#[derive(Debug, Clone, Serialize)]
pub struct OutlineEntry {
    /// Symbol kind.
    pub kind: SymbolKind,
    /// Symbol name.
    pub name: String,
    /// Line number (1-indexed).
    pub line: u32,
    /// Nested child entries.
    pub children: Vec<OutlineEntry>,
}

/// Skeleton view — signatures + docstrings, bodies as `...`.
#[derive(Debug, Clone, Serialize)]
pub struct SkeletonView {
    /// File path, if scoped to a single file.
    pub path: Option<PathBuf>,
    /// Skeleton content with bodies replaced by `...`.
    pub content: String,
    /// Estimated token count.
    pub token_estimate: usize,
}

/// Symbol card — full source of one symbol + parents + callers.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolCardView {
    /// File containing this symbol.
    pub path: PathBuf,
    /// Fully qualified name.
    pub qualified_name: String,
    /// Symbol kind.
    pub kind: SymbolKind,
    /// Signature line.
    pub signature: Option<String>,
    /// Doc comment.
    pub doc: Option<String>,
    /// Full source body.
    pub body: String,
    /// Start and end lines (1-indexed).
    pub line_range: (u32, u32),
    /// Enclosing parent symbols from outermost to innermost.
    pub parent_chain: Vec<String>,
    /// Incoming references (callers).
    pub callers: Vec<RefEntry>,
    /// Outgoing references (callees).
    pub callees: Vec<RefEntry>,
}

/// A reference entry (compact).
#[derive(Debug, Clone, Serialize)]
pub struct RefEntry {
    /// File path.
    pub path: PathBuf,
    /// Line number (1-indexed).
    pub line: u32,
    /// Column number (0-indexed), if known.
    pub col: Option<u32>,
    /// Source line snippet.
    pub snippet: String,
    /// Name of the enclosing symbol, if resolved.
    pub enclosing_symbol: Option<String>,
}

/// Reference digest — count + top-k sites.
#[derive(Debug, Clone, Serialize)]
pub struct RefDigestView {
    /// Symbol name being queried.
    pub symbol: String,
    /// Total number of references found.
    pub total: usize,
    /// Top-k reference entries by weight.
    pub top_k: Vec<RefEntry>,
}

/// Diagnostic messages.
#[derive(Debug, Clone, Serialize)]
pub struct DiagView {
    /// Diagnostic entries.
    pub entries: Vec<DiagEntry>,
}

/// A single diagnostic.
#[derive(Debug, Clone, Serialize)]
pub struct DiagEntry {
    /// File path.
    pub path: PathBuf,
    /// Line number (1-indexed).
    pub line: u32,
    /// Column number (0-indexed).
    pub col: u32,
    /// Diagnostic message text.
    pub message: String,
    /// Severity level.
    pub severity: DiagSeverity,
}

/// Diagnostic severity level.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagSeverity {
    /// Error.
    Error,
    /// Warning.
    Warning,
    /// Informational.
    Info,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outline_serde() {
        let view = ToolOutput::Outline(OutlineView {
            path: PathBuf::from("src/lib.rs"),
            entries: vec![OutlineEntry {
                kind: SymbolKind::Function,
                name: "main".into(),
                line: 1,
                children: vec![],
            }],
        });
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"shape\":\"Outline\""));
        assert!(json.contains("\"main\""));
    }

    #[test]
    fn skeleton_serde() {
        let view = ToolOutput::Skeleton(SkeletonView {
            path: Some(PathBuf::from("src/lib.rs")),
            content: "fn main() { ... }".into(),
            token_estimate: 10,
        });
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"shape\":\"Skeleton\""));
    }

    #[test]
    fn ref_digest_serde() {
        let view = ToolOutput::ReferenceDigest(RefDigestView {
            symbol: "Foo::bar".into(),
            total: 42,
            top_k: vec![RefEntry {
                path: PathBuf::from("src/main.rs"),
                line: 10,
                col: Some(5),
                snippet: "let x = Foo::bar();".into(),
                enclosing_symbol: Some("main".into()),
            }],
        });
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"total\":42"));
    }
}
