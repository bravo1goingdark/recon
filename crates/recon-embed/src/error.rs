//! Error types for the embedding and vector store layer.

/// Errors from embedding or vector store operations.
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    /// Error from the embedding model (fastembed / ONNX).
    #[error("embedding model error: {0}")]
    Model(String),
    /// Error from the vector store (sqlite-vec).
    #[error("vector store error: {0}")]
    Store(String),
    /// I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
