//! Language grammar registry.

use recon_core::lang::Language;
use tree_sitter::Language as TsLanguage;

/// Get the tree-sitter language grammar for a given language.
pub fn ts_language(lang: Language) -> Option<TsLanguage> {
    match lang {
        Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
        Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        Language::Tsx => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        Language::Go => Some(tree_sitter_go::LANGUAGE.into()),
        Language::Java => Some(tree_sitter_java::LANGUAGE.into()),
        Language::C => Some(tree_sitter_c::LANGUAGE.into()),
        Language::Cpp => Some(tree_sitter_cpp::LANGUAGE.into()),
        Language::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_known_languages_have_grammars() {
        let langs = [
            Language::Rust,
            Language::Python,
            Language::TypeScript,
            Language::Tsx,
            Language::JavaScript,
            Language::Go,
            Language::Java,
            Language::C,
            Language::Cpp,
        ];
        for lang in &langs {
            assert!(ts_language(*lang).is_some(), "{lang} missing grammar");
        }
    }

    #[test]
    fn unknown_returns_none() {
        assert!(ts_language(Language::Unknown).is_none());
    }
}
