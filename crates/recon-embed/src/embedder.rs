//! Local ONNX embedding engine wrapping fastembed.
//!
//! Uses `jina-embeddings-v2-base-code` (768-d, Apache-2.0) for code-aware
//! embeddings. Runs entirely on CPU — no GPU or cloud API required.

use crate::error::EmbedError;

/// Local ONNX embedding engine.
pub struct Embedder {
    model: fastembed::TextEmbedding,
}

impl Embedder {
    /// Initialize the embedder from a local HuggingFace model directory.
    ///
    /// Expects the standard directory layout:
    /// - `model.onnx` — ONNX model weights
    /// - `tokenizer.json` — HuggingFace tokenizer
    /// - `tokenizer_config.json` — tokenizer configuration
    /// - `special_tokens_map.json` — special tokens map
    /// - `config.json` — model config (optional but recommended)
    ///
    /// Set `RECON_EMBED_DIR` to a directory with this layout to use a local model
    /// instead of downloading the default (~300 MB).
    pub fn from_local_model(model_dir: &std::path::Path) -> Result<Self, EmbedError> {
        let read = |name: &str| -> Result<Vec<u8>, EmbedError> {
            std::fs::read(model_dir.join(name))
                .map_err(|e| EmbedError::Model(format!("read {name}: {e}")))
        };

        let onnx_file = read("model.onnx")?;
        let tokenizer_file = read("tokenizer.json")?;
        let tokenizer_config_file = read("tokenizer_config.json")?;
        let special_tokens_map_file = read("special_tokens_map.json")?;
        let config_file = std::fs::read(model_dir.join("config.json")).unwrap_or_default();

        let tokenizer_files = fastembed::TokenizerFiles {
            tokenizer_file,
            config_file,
            special_tokens_map_file,
            tokenizer_config_file,
        };

        let model_def = fastembed::UserDefinedEmbeddingModel::new(onnx_file, tokenizer_files)
            .with_pooling(fastembed::Pooling::Mean);

        let model = fastembed::TextEmbedding::try_new_from_user_defined(
            model_def,
            fastembed::InitOptionsUserDefined::new(),
        )
        .map_err(|e| EmbedError::Model(format!("init: {}", e)))?;

        Ok(Self { model })
    }

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

    // format_symbol moved to crate::format::format_symbol (always
    // available, doesn't pull fastembed). The hosted backend reuses
    // the same helper so both code paths embed identical input text.
}

#[cfg(test)]
mod tests {
    use super::*;

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
