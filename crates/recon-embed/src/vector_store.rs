//! LanceDB-backed vector store with scalar filtering.
//!
//! Stores symbol embeddings keyed by `body_hash` for content-addressable
//! caching. Supports vector similarity search with optional language filter.

use crate::error::EmbedError;
use arrow_array::{
    ArrayRef, FixedSizeListArray, Float32Array, LargeBinaryArray, RecordBatch, StringArray,
    UInt64Array,
};
use arrow_schema::{DataType, Field};
use lancedb::query::{ExecutableQuery, QueryBase};
use std::path::Path;
use std::sync::Arc;

const TABLE_NAME: &str = "symbol_embeddings";
const VECTOR_DIM: i32 = 768;

/// An entry to upsert into the vector store.
pub struct EmbedEntry {
    /// Symbol ID (from SQLite).
    pub id: u64,
    /// Qualified symbol name.
    pub qualified_name: String,
    /// Embedding vector (768-d for jina-v2-base-code).
    pub vector: Vec<f32>,
    /// blake3 hash of the symbol body (cache key).
    pub body_hash: Vec<u8>,
    /// Language name for scalar filtering.
    pub lang: String,
}

/// LanceDB-backed vector store.
pub struct VectorStore {
    db: lancedb::Connection,
}

fn build_batch(entries: &[EmbedEntry]) -> Result<RecordBatch, EmbedError> {
    let ids: Vec<u64> = entries.iter().map(|e| e.id).collect();
    let names: Vec<&str> = entries.iter().map(|e| e.qualified_name.as_str()).collect();
    let langs: Vec<&str> = entries.iter().map(|e| e.lang.as_str()).collect();

    // Build FixedSizeList(Float32, 768) for the vector column
    let flat: Vec<f32> = entries
        .iter()
        .flat_map(|e| e.vector.iter().copied())
        .collect();
    let values = Arc::new(Float32Array::from(flat)) as ArrayRef;
    let list_field = Arc::new(Field::new("item", DataType::Float32, true));
    let vectors = FixedSizeListArray::try_new(list_field, VECTOR_DIM, values, None)
        .map_err(|e| EmbedError::Store(format!("arrow vector: {e}")))?;

    let hashes = LargeBinaryArray::from_iter_values(entries.iter().map(|e| e.body_hash.as_slice()));

    RecordBatch::try_from_iter(vec![
        ("id", Arc::new(UInt64Array::from(ids)) as ArrayRef),
        (
            "qualified_name",
            Arc::new(StringArray::from(names)) as ArrayRef,
        ),
        ("vector", Arc::new(vectors) as ArrayRef),
        ("body_hash", Arc::new(hashes) as ArrayRef),
        ("lang", Arc::new(StringArray::from(langs)) as ArrayRef),
    ])
    .map_err(|e| EmbedError::Store(format!("record batch: {e}")))
}

impl VectorStore {
    /// Open or create a vector store at the given directory.
    pub async fn open(path: &Path) -> Result<Self, EmbedError> {
        let db = lancedb::connect(path.to_string_lossy().as_ref())
            .execute()
            .await
            .map_err(|e| EmbedError::Store(format!("open: {e}")))?;
        Ok(Self { db })
    }

    /// Upsert embedding entries. Creates the table on first call.
    pub async fn upsert_embeddings(&self, entries: &[EmbedEntry]) -> Result<(), EmbedError> {
        if entries.is_empty() {
            return Ok(());
        }

        let batch = build_batch(entries)?;

        let tables = self
            .db
            .table_names()
            .execute()
            .await
            .map_err(|e| EmbedError::Store(format!("list tables: {e}")))?;

        if tables.iter().any(|t| t == TABLE_NAME) {
            let table = self
                .db
                .open_table(TABLE_NAME)
                .execute()
                .await
                .map_err(|e| EmbedError::Store(format!("open table: {e}")))?;
            table
                .add(batch)
                .execute()
                .await
                .map_err(|e| EmbedError::Store(format!("add: {e}")))?;
        } else {
            self.db
                .create_table(TABLE_NAME, batch)
                .execute()
                .await
                .map_err(|e| EmbedError::Store(format!("create table: {e}")))?;
        }

        Ok(())
    }

    /// Vector similarity search with optional language filter.
    ///
    /// Returns `(symbol_id, distance)` pairs sorted by relevance (lower distance = closer).
    pub async fn search(
        &self,
        query_vector: Vec<f32>,
        lang_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(u64, f32)>, EmbedError> {
        let tables = self
            .db
            .table_names()
            .execute()
            .await
            .map_err(|e| EmbedError::Store(format!("list tables: {e}")))?;

        if !tables.iter().any(|t| t == TABLE_NAME) {
            return Ok(Vec::new());
        }

        let table = self
            .db
            .open_table(TABLE_NAME)
            .execute()
            .await
            .map_err(|e| EmbedError::Store(format!("open table: {e}")))?;

        let mut query = table
            .vector_search(query_vector)
            .map_err(|e| EmbedError::Store(format!("vector search: {e}")))?
            .limit(limit);

        if let Some(lang) = lang_filter {
            query = query.only_if(format!("lang = '{lang}'"));
        }

        use futures::TryStreamExt;
        let batches: Vec<RecordBatch> = query
            .execute()
            .await
            .map_err(|e| EmbedError::Store(format!("execute: {e}")))?
            .try_collect()
            .await
            .map_err(|e| EmbedError::Store(format!("collect: {e}")))?;

        let mut hits = Vec::new();
        for batch in &batches {
            let ids = batch
                .column_by_name("id")
                .and_then(|c| c.as_any().downcast_ref::<UInt64Array>());
            let scores = batch
                .column_by_name("_distance")
                .and_then(|c| c.as_any().downcast_ref::<Float32Array>());

            if let (Some(ids), Some(scores)) = (ids, scores) {
                for i in 0..ids.len() {
                    hits.push((ids.value(i), scores.value(i)));
                }
            }
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_and_upsert() {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::open(dir.path()).await.unwrap();

        let entries = vec![
            EmbedEntry {
                id: 1,
                qualified_name: "crate::foo".into(),
                vector: vec![0.1; 768],
                body_hash: vec![0u8; 32],
                lang: "rust".into(),
            },
            EmbedEntry {
                id: 2,
                qualified_name: "crate::bar".into(),
                vector: vec![0.2; 768],
                body_hash: vec![1u8; 32],
                lang: "rust".into(),
            },
        ];

        store.upsert_embeddings(&entries).await.unwrap();

        let results = store.search(vec![0.1; 768], None, 10).await.unwrap();
        assert!(!results.is_empty(), "should find entries");
    }

    #[tokio::test]
    async fn search_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::open(dir.path()).await.unwrap();
        let results = store.search(vec![0.1; 768], None, 10).await.unwrap();
        assert!(results.is_empty());
    }
}
