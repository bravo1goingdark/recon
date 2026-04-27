//! The canonical output shapes for MCP tool responses.

use crate::symbol::SymbolKind;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
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
    /// Structured tool-error response. Agents pattern-match on `code` / `kind`
    /// rather than scraping an opaque "Error: …" prefix from the free-text body.
    Error(ToolErrorView),
}

/// Structured tool-error response.
///
/// Wire shape matches JSON-RPC's `error` object: a stable numeric `code`,
/// a human message, optional structured `data`, plus a `request_id` clients
/// can cite in support tickets and recon can grep out of logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolErrorView {
    /// Stable numeric code in `-32001..=-32099`. See `recon_core::error::ReconErrorCode`.
    pub code: i32,
    /// Kebab-case identifier (`not_found`, `timeout`, …) for switch-style handling.
    pub kind: CompactString,
    /// User-facing message. Safe to display verbatim; secrets already redacted.
    pub message: String,
    /// Optional structured payload (e.g. the path that failed, size in bytes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// ULID assigned at tool-entry — use to correlate with server logs.
    pub request_id: CompactString,
}

/// Outline view — one line per top-level symbol.
#[derive(Debug, Clone, Serialize)]
pub struct OutlineView {
    /// File path.
    pub path: PathBuf,
    /// Top-level symbol entries.
    pub entries: SmallVec<[OutlineEntry; 4]>,
}

/// A single entry in an outline.
///
/// `children` is omitted from JSON when empty — leaf entries (most of them)
/// previously emitted `"children":[]`, ~14 bytes per leaf × tens of leaves
/// per file outline.
#[derive(Debug, Clone, Serialize)]
pub struct OutlineEntry {
    /// Symbol kind.
    pub kind: SymbolKind,
    /// Symbol name.
    pub name: CompactString,
    /// Line number (1-indexed).
    pub line: u32,
    /// Nested child entries.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<OutlineEntry>,
}

/// Skeleton view — signatures + docstrings, bodies as `...`.
#[derive(Debug, Clone, Serialize)]
pub struct SkeletonView {
    /// File path, if scoped to a single file. Omitted from JSON when `None`
    /// (e.g. repo-map output that aggregates many files).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Skeleton content with bodies replaced by `...`.
    pub content: String,
    /// Estimated token count.
    pub token_estimate: usize,
}

/// Symbol card — full source of one symbol + parents + callers.
///
/// `signature`, `doc`, and the three list fields (`parent_chain`, `callers`,
/// `callees`) are omitted from JSON when empty. For symbols with no resolved
/// callers/callees this saves ~26 bytes per response (`"callers":[],"callees":[]`)
/// — multiplied across `code_read_symbol` calls in a session it's a measurable
/// token win.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolCardView {
    /// File containing this symbol.
    pub path: PathBuf,
    /// Fully qualified name.
    pub qualified_name: String,
    /// Symbol kind.
    pub kind: SymbolKind,
    /// Signature line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Doc comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    /// Full source body.
    pub body: String,
    /// Start and end lines (1-indexed).
    pub line_range: (u32, u32),
    /// Enclosing parent symbols from outermost to innermost.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parent_chain: Vec<String>,
    /// Incoming references (callers).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callers: Vec<RefEntry>,
    /// Outgoing references (callees).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callees: Vec<RefEntry>,
}

/// A reference entry (compact).
///
/// `col` and `enclosing_symbol` are omitted from JSON when not captured —
/// most lexical hits don't carry a column or a resolved enclosing symbol,
/// and `"col":null` on every hit is pure overhead.
#[derive(Debug, Clone, Serialize)]
pub struct RefEntry {
    /// File path.
    pub path: PathBuf,
    /// Line number (1-indexed).
    pub line: u32,
    /// Column number (0-indexed), if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col: Option<u32>,
    /// Source line snippet.
    pub snippet: CompactString,
    /// Name of the enclosing symbol, if resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enclosing_symbol: Option<CompactString>,
}

/// Reference digest — count + top-k sites.
#[derive(Debug, Clone, Serialize)]
pub struct RefDigestView {
    /// Symbol name being queried.
    pub symbol: CompactString,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
    use smallvec::smallvec;

    #[test]
    fn outline_serde() {
        let view = ToolOutput::Outline(OutlineView {
            path: PathBuf::from("src/lib.rs"),
            entries: smallvec![OutlineEntry {
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
    fn outline_empty_entries_serde() {
        let view = ToolOutput::Outline(OutlineView {
            path: PathBuf::from("src/empty.rs"),
            entries: SmallVec::new(),
        });
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"entries\":[]"));
    }

    #[test]
    fn outline_nested_children() {
        let child = OutlineEntry {
            kind: SymbolKind::Method,
            name: "new".into(),
            line: 5,
            children: vec![],
        };
        let parent = OutlineEntry {
            kind: SymbolKind::Struct,
            name: "Foo".into(),
            line: 1,
            children: vec![child],
        };
        let json = serde_json::to_string(&parent).unwrap();
        assert!(json.contains("\"Foo\""));
        assert!(json.contains("\"new\""));
        assert!(json.contains("\"method\""));
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

    #[test]
    fn ref_entry_omits_none_fields() {
        // Token-saving guarantee (v0.2.2): None fields must NOT serialize.
        // Inverted from the v0.2.1-and-earlier behavior that emitted
        // `"col":null` and `"enclosing_symbol":null` on every lexical hit.
        let entry = RefEntry {
            path: PathBuf::from("src/lib.rs"),
            line: 3,
            col: None,
            snippet: "foo()".into(),
            enclosing_symbol: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            !json.contains("\"col\""),
            "col=None must be omitted: {json}"
        );
        assert!(
            !json.contains("\"enclosing_symbol\""),
            "enclosing_symbol=None must be omitted: {json}"
        );
        // Required fields still present.
        assert!(json.contains("\"path\""));
        assert!(json.contains("\"line\":3"));
        assert!(json.contains("\"snippet\":\"foo()\""));
    }

    #[test]
    fn ref_entry_includes_some_fields() {
        let entry = RefEntry {
            path: PathBuf::from("src/lib.rs"),
            line: 3,
            col: Some(7),
            snippet: "foo()".into(),
            enclosing_symbol: Some("main".into()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"col\":7"));
        assert!(json.contains("\"enclosing_symbol\":\"main\""));
    }

    #[test]
    fn outline_entry_omits_empty_children() {
        let leaf = OutlineEntry {
            kind: SymbolKind::Function,
            name: "main".into(),
            line: 1,
            children: vec![],
        };
        let json = serde_json::to_string(&leaf).unwrap();
        assert!(
            !json.contains("\"children\""),
            "empty children must be omitted: {json}"
        );
    }

    #[test]
    fn symbol_card_view_omits_empty_lists_and_none_fields() {
        let view = SymbolCardView {
            path: PathBuf::from("src/lib.rs"),
            qualified_name: "lib::standalone".into(),
            kind: SymbolKind::Function,
            signature: None,
            doc: None,
            body: "fn standalone() {}".into(),
            line_range: (1, 1),
            parent_chain: vec![],
            callers: vec![],
            callees: vec![],
        };
        let json = serde_json::to_string(&view).unwrap();
        for field in &[
            "\"signature\"",
            "\"doc\"",
            "\"parent_chain\"",
            "\"callers\"",
            "\"callees\"",
        ] {
            assert!(!json.contains(field), "{field} must be omitted: {json}");
        }
    }

    #[test]
    fn skeleton_view_omits_none_path() {
        let view = SkeletonView {
            path: None,
            content: "...".into(),
            token_estimate: 1,
        };
        let json = serde_json::to_string(&view).unwrap();
        assert!(
            !json.contains("\"path\""),
            "None path must be omitted: {json}"
        );
    }

    #[test]
    fn compact_string_inline_storage() {
        // Names up to 23 bytes are stored inline without heap allocation.
        let entry = OutlineEntry {
            kind: SymbolKind::Function,
            name: "short_name".into(),
            line: 1,
            children: vec![],
        };
        assert_eq!(entry.name.as_str(), "short_name");
    }

    #[test]
    fn symbol_card_view_serde() {
        let view = ToolOutput::SymbolCard(SymbolCardView {
            path: PathBuf::from("src/auth.rs"),
            qualified_name: "auth::validate".into(),
            kind: SymbolKind::Function,
            signature: Some("pub fn validate(token: &str) -> bool".into()),
            doc: Some("Validates an auth token.".into()),
            body: "pub fn validate(token: &str) -> bool {\n    true\n}".into(),
            line_range: (10, 12),
            parent_chain: vec!["mod auth".into()],
            callers: vec![RefEntry {
                path: PathBuf::from("src/main.rs"),
                line: 5,
                col: Some(10),
                snippet: "validate(tok)".into(),
                enclosing_symbol: Some("main".into()),
            }],
            callees: vec![RefEntry {
                path: PathBuf::from("src/auth.rs"),
                line: 11,
                col: None,
                snippet: "parse_token".into(),
                enclosing_symbol: None,
            }],
        });
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"shape\":\"SymbolCard\""));
        assert!(json.contains("\"auth::validate\""));
        assert!(json.contains("\"parent_chain\""));
        assert!(json.contains("\"callers\""));
        assert!(json.contains("\"callees\""));
    }

    #[test]
    fn diagnostics_serde() {
        let view = ToolOutput::Diagnostics(DiagView {
            entries: vec![
                DiagEntry {
                    path: PathBuf::from("src/lib.rs"),
                    line: 42,
                    col: 5,
                    message: "unused variable".into(),
                    severity: DiagSeverity::Warning,
                },
                DiagEntry {
                    path: PathBuf::from("src/lib.rs"),
                    line: 50,
                    col: 0,
                    message: "cannot find value".into(),
                    severity: DiagSeverity::Error,
                },
            ],
        });
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"shape\":\"Diagnostics\""));
        assert!(json.contains("\"warning\""));
        assert!(json.contains("\"error\""));
        assert!(json.contains("\"unused variable\""));
    }

    #[test]
    fn diag_severity_serde_variants() {
        assert_eq!(
            serde_json::to_string(&DiagSeverity::Error).unwrap(),
            "\"error\""
        );
        assert_eq!(
            serde_json::to_string(&DiagSeverity::Warning).unwrap(),
            "\"warning\""
        );
        assert_eq!(
            serde_json::to_string(&DiagSeverity::Info).unwrap(),
            "\"info\""
        );
    }

    #[test]
    fn diag_entry_roundtrip() {
        let entry = DiagEntry {
            path: PathBuf::from("tests/test.rs"),
            line: 100,
            col: 12,
            message: "assertion failed".into(),
            severity: DiagSeverity::Error,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: DiagEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.line, 100);
        assert_eq!(back.message, "assertion failed");
        assert_eq!(back.severity, DiagSeverity::Error);
    }

    #[test]
    fn empty_diagnostics() {
        let view = ToolOutput::Diagnostics(DiagView { entries: vec![] });
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"entries\":[]"));
    }
}
