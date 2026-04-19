//! Local ONNX embedding engine wrapping fastembed.
//!
//! Uses `jina-embeddings-v2-base-code` (768-d, Apache-2.0) for code-aware
//! embeddings. Runs entirely on CPU — no GPU or cloud API required.

use crate::error::EmbedError;
use recon_core::symbol::Symbol;

/// Local ONNX embedding engine.
pub struct Embedder {
    model: fastembed::TextEmbedding,
}

impl Embedder {
    /// Initialize the embedder. Downloads the model on first run (~300 MB).
    pub fn new() -> Result<Self, EmbedError> {
        let model = fastembed::TextEmbedding::try_new(
            fastembed::InitOptions::new(fastembed::EmbeddingModel::JinaEmbeddingsV2BaseCode)
                .with_show_download_progress(true),
        )
        .map_err(|e| EmbedError::Model(format!("init: {e}")))?;

        Ok(Self { model })
    }

    /// Embed a batch of text passages. Batch size 32-64 is optimal for throughput.
    pub fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.model
            .embed(texts, None)
            .map_err(|e| EmbedError::Model(format!("embed: {e}")))
    }

    /// Embed a single text passage.
    pub fn embed_one(&mut self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let results = self
            .model
            .embed(vec![text.to_string()], None)
            .map_err(|e| EmbedError::Model(format!("embed: {e}")))?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| EmbedError::Model("empty result".into()))
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use compact_str::CompactString;
    use recon_core::lang::Language;
    use recon_core::symbol::SymbolKind;
    use std::path::PathBuf;

    #[test]
    fn format_symbol_works() {
        let sym = Symbol {
            id: 1,
            path: PathBuf::from("src/lib.rs"),
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
        let formatted = Embedder::format_symbol(&sym, "{ email.contains('@') }");
        assert!(formatted.contains("Rust"));
        assert!(formatted.contains("fn"));
        assert!(formatted.contains("crate::validate"));
        assert!(formatted.contains("Validate an email"));
        assert!(formatted.contains("email.contains"));
    }

    #[test]
    #[ignore] // Requires model download (~300 MB)
    fn embed_batch_works() {
        let mut embedder = Embedder::new().unwrap();
        let texts = vec![
            "fn main() {}".to_string(),
            "struct Foo { x: i32 }".to_string(),
        ];
        let vectors = embedder.embed_batch(&texts).unwrap();
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors[0].len(), 768);
    }
}
