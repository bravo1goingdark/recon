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
    /// Bundled context envelope populated by `code_context`.
    ///
    /// When omitted (the common case for `code_read_symbol`), the response
    /// is byte-identical to v0.2.x. When present, it carries up to a few
    /// each of caller/callee/type/test summaries that the agent would
    /// otherwise need 4+ separate tool calls to assemble.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextEnvelope>,
}

/// Bundled context for a symbol — replaces the canonical 4-call
/// "understand X" loop with a single response.
///
/// Sections are emitted only when non-empty. The sum of section sizes is
/// bounded by the requesting tool's `token_budget` argument; sections are
/// dropped in priority order (tests → callees → types → callers) when the
/// bundle would exceed budget. The `truncated` flag marks any drop.
#[derive(Debug, Clone, Serialize)]
pub struct ContextEnvelope {
    /// Up to N immediate callers, ranked by importance.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callers: Vec<SymbolHop>,
    /// Up to N immediate callees.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callees: Vec<SymbolHop>,
    /// Top-K types this symbol depends on (struct/class/trait/enum returns
    /// or parameters).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<SymbolHop>,
    /// Up to N tests that exercise this symbol (transitive caller-of-test).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<SymbolHop>,
    /// True if any section was dropped to fit the token budget.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
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

/// Reference digest — count + top-k sites, optionally a graph path or tiers.
///
/// Three modes share this view to keep the tool surface inside the canonical
/// 5-shape envelope:
///
/// 1. `code_find_refs` — populates `total` + `top_k` only.
/// 2. `code_path` — populates `path` (ordered hop sequence). `top_k` is
///    omitted; `total` is the path length.
/// 3. `code_callers` / `code_callees` / `code_impact` — populate `tiers`
///    (one per BFS ring). `top_k` is omitted; `total` is the sum of
///    distinct nodes across all tiers.
///
/// All extension fields are skip-when-empty — a `code_find_refs` response
/// is byte-identical to v0.2.x.
#[derive(Debug, Clone, Serialize)]
pub struct RefDigestView {
    /// Symbol name being queried.
    pub symbol: CompactString,
    /// Total number of references / nodes / hops, depending on mode.
    pub total: usize,
    /// Top-k reference entries by weight (lexical mode only). Always emitted
    /// — preserves byte-identical output for `code_find_refs` clients that
    /// depend on the field's presence.
    pub top_k: Vec<RefEntry>,
    /// Ordered hop sequence — `path[0]` is the source, `path[last]` is the
    /// destination. Used by `code_path`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<SymbolHop>,
    /// Layered BFS tiers — `tiers[k]` is the (k+1)-th ring of callers /
    /// callees. Used by `code_callers`, `code_callees`, `code_impact`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<RefTier>,
    /// True if the underlying traversal was capped (visit-limit or per-tier
    /// fan-out). Agents should treat the result as a partial answer rather
    /// than a definitive set when this is set.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Best-effort hint about why a path was not found. Populated by
    /// `code_path` when the BFS reached an unresolved boundary (likely
    /// dynamic dispatch, FFI, or external functions). Format:
    /// `"unresolved near <qname>"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unresolved_hint: Option<String>,
    /// Layered list of test-symbol hops that exercise the queried symbol.
    /// Populated by `code_impact` only — the rest of the tools omit it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<SymbolHop>,
}

/// One hop in a graph-traversal result — a symbol identified by its
/// qualified name + file location.
///
/// This is intentionally distinct from `RefEntry`: a [`RefEntry`] describes
/// a *call site* (a single line in some caller's body), while a
/// [`SymbolHop`] describes a *symbol definition* (the function being
/// called, with its declaration line). Graph tools traffic in symbol
/// identities; lexical search traffics in call sites.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolHop {
    /// Fully qualified name (e.g. `crate::auth::validate`).
    pub qualified_name: String,
    /// Symbol kind — helps the agent disambiguate fn vs method vs trait.
    pub kind: SymbolKind,
    /// Source file containing the symbol.
    pub path: PathBuf,
    /// Declaration line (1-indexed).
    pub line: u32,
}

/// One ring of a layered BFS — depth + the nodes reached at that depth.
#[derive(Debug, Clone, Serialize)]
pub struct RefTier {
    /// Hops from the seed (1 = direct callers/callees, 2 = next ring, ...).
    pub depth: u32,
    /// Symbol hops in this tier.
    pub refs: Vec<SymbolHop>,
    /// True if this tier was capped at the per-tier fan-out limit.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/// Helper for `serde(skip_serializing_if)` on `bool` fields — omit when false.
#[inline]
fn is_false(b: &bool) -> bool {
    !*b
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
            path: vec![],
            tiers: vec![],
            truncated: false,
            unresolved_hint: None,
            tests: vec![],
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
            context: None,
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
            context: None,
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

    #[test]
    fn ref_digest_legacy_shape_byte_identical() {
        // A `code_find_refs`-style payload — none of the new graph fields set.
        // Must serialize without `path`, `tiers`, `truncated`, `unresolved_hint`,
        // or `tests` keys so v0.2.x clients see identical bytes. (Substring
        // search would false-match `RefEntry.path` inside `top_k`, so we parse
        // the JSON and assert on top-level keys only.)
        let view = RefDigestView {
            symbol: "foo".into(),
            total: 3,
            top_k: vec![RefEntry {
                path: PathBuf::from("src/lib.rs"),
                line: 10,
                col: None,
                snippet: "foo()".into(),
                enclosing_symbol: None,
            }],
            path: vec![],
            tiers: vec![],
            truncated: false,
            unresolved_hint: None,
            tests: vec![],
        };
        let json = serde_json::to_string(&view).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = parsed.as_object().expect("RefDigest serializes as object");
        for ghost in &["path", "tiers", "truncated", "unresolved_hint", "tests"] {
            assert!(
                !obj.contains_key(*ghost),
                "{ghost} must be omitted at top level when empty: {json}"
            );
        }
        assert!(obj.contains_key("top_k"));
        assert!(obj.contains_key("symbol"));
        assert!(obj.contains_key("total"));
    }

    #[test]
    fn ref_digest_path_mode_serde() {
        // `top_k` is always emitted (even when empty) — see RefDigestView
        // doc. The `path` field is what distinguishes path-mode responses.
        let view = RefDigestView {
            symbol: "main".into(),
            total: 3,
            top_k: vec![],
            path: vec![
                SymbolHop {
                    qualified_name: "crate::main".into(),
                    kind: SymbolKind::Function,
                    path: PathBuf::from("src/main.rs"),
                    line: 1,
                },
                SymbolHop {
                    qualified_name: "crate::init".into(),
                    kind: SymbolKind::Function,
                    path: PathBuf::from("src/init.rs"),
                    line: 1,
                },
                SymbolHop {
                    qualified_name: "crate::config_load".into(),
                    kind: SymbolKind::Function,
                    path: PathBuf::from("src/config.rs"),
                    line: 1,
                },
            ],
            tiers: vec![],
            truncated: false,
            unresolved_hint: None,
            tests: vec![],
        };
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"path\""));
        assert!(!json.contains("\"tiers\""));
    }

    #[test]
    fn ref_digest_tiers_mode_serde() {
        let view = RefDigestView {
            symbol: "process".into(),
            total: 2,
            top_k: vec![],
            path: vec![],
            tiers: vec![RefTier {
                depth: 1,
                refs: vec![SymbolHop {
                    qualified_name: "crate::caller".into(),
                    kind: SymbolKind::Function,
                    path: PathBuf::from("src/caller.rs"),
                    line: 5,
                }],
                truncated: true,
            }],
            truncated: true,
            unresolved_hint: None,
            tests: vec![],
        };
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"tiers\""));
        assert!(json.contains("\"depth\":1"));
        assert!(json.contains("\"truncated\":true"));
    }

    #[test]
    fn symbol_card_context_envelope_serde() {
        let view = SymbolCardView {
            path: PathBuf::from("src/auth.rs"),
            qualified_name: "auth::validate".into(),
            kind: SymbolKind::Function,
            signature: Some("pub fn validate(token: &str) -> bool".into()),
            doc: None,
            body: "...".into(),
            line_range: (10, 12),
            parent_chain: vec![],
            callers: vec![],
            callees: vec![],
            context: Some(ContextEnvelope {
                callers: vec![SymbolHop {
                    qualified_name: "main".into(),
                    kind: SymbolKind::Function,
                    path: PathBuf::from("src/main.rs"),
                    line: 5,
                }],
                callees: vec![],
                types: vec![],
                tests: vec![SymbolHop {
                    qualified_name: "tests::test_validate".into(),
                    kind: SymbolKind::Function,
                    path: PathBuf::from("src/auth.rs"),
                    line: 100,
                }],
                truncated: false,
            }),
        };
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"context\""));
        assert!(json.contains("\"tests\""));
        // truncated=false on the envelope must be omitted
        let envelope_chunk = json.split("\"context\":").nth(1).unwrap();
        assert!(
            !envelope_chunk.starts_with("{\"callers\":[],"),
            "empty inner sections must be omitted"
        );
    }

    #[test]
    fn symbol_card_context_omitted_when_none() {
        let view = SymbolCardView {
            path: PathBuf::from("src/x.rs"),
            qualified_name: "x".into(),
            kind: SymbolKind::Function,
            signature: None,
            doc: None,
            body: "fn x(){}".into(),
            line_range: (1, 1),
            parent_chain: vec![],
            callers: vec![],
            callees: vec![],
            context: None,
        };
        let json = serde_json::to_string(&view).unwrap();
        assert!(
            !json.contains("\"context\""),
            "context=None must be omitted: {json}"
        );
    }
}
