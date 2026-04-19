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
    pub path: PathBuf,
    pub entries: Vec<OutlineEntry>,
}

/// A single entry in an outline.
#[derive(Debug, Clone, Serialize)]
pub struct OutlineEntry {
    pub kind: SymbolKind,
    pub name: String,
    pub line: u32,
    pub children: Vec<OutlineEntry>,
}

/// Skeleton view — signatures + docstrings, bodies as `...`.
#[derive(Debug, Clone, Serialize)]
pub struct SkeletonView {
    pub path: Option<PathBuf>,
    pub content: String,
    pub token_estimate: usize,
}

/// Symbol card — full source of one symbol + parents + callers.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolCardView {
    pub path: PathBuf,
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub signature: Option<String>,
    pub doc: Option<String>,
    pub body: String,
    pub line_range: (u32, u32),
    pub parent_chain: Vec<String>,
    pub callers: Vec<RefEntry>,
    pub callees: Vec<RefEntry>,
}

/// A reference entry (compact).
#[derive(Debug, Clone, Serialize)]
pub struct RefEntry {
    pub path: PathBuf,
    pub line: u32,
    pub col: Option<u32>,
    pub snippet: String,
    pub enclosing_symbol: Option<String>,
}

/// Reference digest — count + top-k sites.
#[derive(Debug, Clone, Serialize)]
pub struct RefDigestView {
    pub symbol: String,
    pub total: usize,
    pub top_k: Vec<RefEntry>,
}

/// Diagnostic messages.
#[derive(Debug, Clone, Serialize)]
pub struct DiagView {
    pub entries: Vec<DiagEntry>,
}

/// A single diagnostic.
#[derive(Debug, Clone, Serialize)]
pub struct DiagEntry {
    pub path: PathBuf,
    pub line: u32,
    pub col: u32,
    pub message: String,
    pub severity: DiagSeverity,
}

/// Diagnostic severity level.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagSeverity {
    Error,
    Warning,
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
