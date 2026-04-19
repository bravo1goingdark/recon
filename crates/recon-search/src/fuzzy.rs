//! Fuzzy symbol matching using nucleo.

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Matcher, Utf32Str};
use recon_core::symbol::Symbol;

/// Score and rank symbols against a fuzzy query.
pub fn fuzzy_rank(symbols: &[Symbol], query: &str, limit: usize) -> Vec<(usize, u32)> {
    if query.is_empty() || symbols.is_empty() {
        return Vec::new();
    }

    let pattern = Pattern::new(
        query,
        CaseMatching::Smart,
        Normalization::Smart,
        nucleo::pattern::AtomKind::Fuzzy,
    );
    let mut matcher = Matcher::default();

    let mut buf = Vec::with_capacity(128);
    let mut scored: Vec<(usize, u32)> = Vec::with_capacity(symbols.len().min(256));

    for (idx, sym) in symbols.iter().enumerate() {
        buf.clear();
        let name = sym.name.as_str();
        let haystack = Utf32Str::new(name, &mut buf);
        if let Some(score) = pattern.score(haystack, &mut matcher) {
            scored.push((idx, score));
        }
    }

    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use compact_str::CompactString;
    use recon_core::lang::Language;
    use recon_core::symbol::SymbolKind;
    use std::path::PathBuf;

    fn make_sym(name: &str) -> Symbol {
        Symbol {
            id: 0,
            path: PathBuf::from("test.rs"),
            name: CompactString::new(name),
            qualified_name: CompactString::new(name),
            kind: SymbolKind::Function,
            signature: None,
            doc: None,
            parent_id: None,
            byte_range: 0..10,
            line_range: 1..=1,
            body_hash: [0u8; 32],
            lang: Language::Rust,
        }
    }

    #[test]
    fn fuzzy_finds_close_match() {
        let symbols = vec![
            make_sym("validate_email"),
            make_sym("send_email"),
            make_sym("process_data"),
            make_sym("validate_phone"),
        ];

        let ranked = fuzzy_rank(&symbols, "val_eml", 10);
        assert!(!ranked.is_empty());
        // validate_email should rank highest
        assert_eq!(symbols[ranked[0].0].name.as_str(), "validate_email");
    }

    #[test]
    fn fuzzy_empty_query() {
        let symbols = vec![make_sym("foo")];
        let ranked = fuzzy_rank(&symbols, "", 10);
        assert!(ranked.is_empty());
    }

    #[test]
    fn fuzzy_respects_limit() {
        let symbols: Vec<_> = (0..20).map(|i| make_sym(&format!("func_{i}"))).collect();
        let ranked = fuzzy_rank(&symbols, "func", 5);
        assert!(ranked.len() <= 5);
    }
}
