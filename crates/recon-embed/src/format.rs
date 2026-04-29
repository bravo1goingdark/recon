//! Symbol-text formatting shared by every embed backend.
//!
//! The shape of the input that gets embedded must be identical
//! regardless of whether the backend is local fastembed or the
//! hosted endpoint — otherwise the same code, embedded twice, would
//! land at different vectors and break cache + retrieval semantics.
//! Keep this function tiny and dependency-free so both backends can
//! call it without conditional compilation.

use recon_core::symbol::Symbol;

/// Format a symbol for embedding input.
///
/// Format: `"{language} {kind} {qualified_name}({signature}) {doc}\n{body}"`
pub fn format_symbol(sym: &Symbol, body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 128);
    out.push_str(sym.lang.name());
    out.push(' ');
    out.push_str(sym.kind.label());
    out.push(' ');
    out.push_str(&sym.qualified_name);
    if let Some(sig) = &sym.signature {
        out.push('(');
        out.push_str(sig);
        out.push(')');
    }
    if let Some(doc) = &sym.doc {
        out.push(' ');
        out.push_str(doc);
    }
    out.push('\n');
    out.push_str(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use compact_str::CompactString;
    use recon_core::lang::Language;
    use recon_core::symbol::SymbolKind;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[test]
    fn format_symbol_emits_expected_shape() {
        let sym = Symbol {
            id: 1,
            path: Arc::new(PathBuf::from("src/lib.rs")),
            name: CompactString::new("validate"),
            qualified_name: CompactString::new("crate::validate"),
            kind: SymbolKind::Function,
            signature: Some("fn validate(email: &str) -> bool".into()),
            doc: Some("Validate an email address.".into()),
            parent_id: None,
            byte_range: 0..100,
            line_range: 1..=5,
            body_hash: [0u8; 32],
            lang: Language::Rust,
        };
        let formatted = format_symbol(&sym, "{ email.contains('@') }");
        assert!(formatted.contains("Rust"));
        assert!(formatted.contains("fn"));
        assert!(formatted.contains("crate::validate"));
        assert!(formatted.contains("Validate an email"));
        assert!(formatted.contains("email.contains"));
    }
}
