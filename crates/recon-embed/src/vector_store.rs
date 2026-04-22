//! Write-only sqlite-vec vector store.
//!
//! [`VectorStore`] owns the single write connection to `embed.db` and is
//! moved into the file-watcher thread after initialisation — no locks needed.
//! Read operations (search, hash lookup) live in [`crate::VecReadPool`].

use std::path::Path;

use rusqlite::{params, Connection};

use crate::error::EmbedError;

/// Dimension of the jina-v2-base-code embedding model.
pub(crate) const VECTOR_DIM: usize = 768;

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

/// Write-only sqlite-vec vector store — owns the single writer connection.
///
/// Moved into the watcher thread after [`crate::init_embed`]; never shared,
/// so no locking is required.
pub struct VectorStore {
    conn: Connection,
}

impl VectorStore {
    /// Open or create a vector store at the given directory.
    ///
    /// Registers sqlite-vec globally (idempotent), creates the schema, and
    /// applies WAL pragmas for concurrent reader access.
    pub fn open(path: &Path) -> Result<Self, EmbedError> {
        register_sqlite_vec();
        let db_path = path.join("embed.db");
        let conn =
            Connection::open(&db_path).map_err(|e| EmbedError::Store(format!("open: {e}")))?;
        conn.execute_batch(&format!(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA cache_size=-32000;
             PRAGMA mmap_size=268435456;
             PRAGMA temp_store=MEMORY;
             CREATE VIRTUAL TABLE IF NOT EXISTS symbol_embeddings
                 USING vec0(embedding float[{VECTOR_DIM}]);
             CREATE TABLE IF NOT EXISTS embed_meta (
                 id             INTEGER PRIMARY KEY,
                 qualified_name TEXT    NOT NULL,
                 body_hash      BLOB    NOT NULL,
                 lang           TEXT    NOT NULL
             );"
        ))
        .map_err(|e| EmbedError::Store(format!("migrate: {e}")))?;
        Ok(Self { conn })
    }

    /// Upsert embedding entries in a single transaction.
    ///
    /// `INSERT OR REPLACE` gives true upsert semantics: re-indexing a symbol
    /// replaces its previous vector and metadata.
    pub fn upsert_embeddings(&self, entries: &[EmbedEntry]) -> Result<(), EmbedError> {
        if entries.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| EmbedError::Store(format!("begin tx: {e}")))?;
        for entry in entries {
            let vec_bytes = f32_to_le_bytes(&entry.vector);
            tx.execute(
                "INSERT OR REPLACE INTO symbol_embeddings(rowid, embedding) VALUES (?1, ?2)",
                params![entry.id as i64, vec_bytes],
            )
            .map_err(|e| EmbedError::Store(format!("insert vec: {e}")))?;
            tx.execute(
                "INSERT OR REPLACE INTO embed_meta(id, qualified_name, body_hash, lang) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    entry.id as i64,
                    &entry.qualified_name,
                    &entry.body_hash,
                    &entry.lang
                ],
            )
            .map_err(|e| EmbedError::Store(format!("insert meta: {e}")))?;
        }
        tx.commit()
            .map_err(|e| EmbedError::Store(format!("commit: {e}")))?;
        Ok(())
    }
}

/// Register sqlite-vec as a global SQLite auto-extension.
///
/// Idempotent — SQLite deduplicates registrations of the same entry point.
/// Called by both [`VectorStore::open`] and [`crate::VecReadPool::new`].
#[allow(clippy::missing_transmute_annotations)]
pub(crate) fn register_sqlite_vec() {
    // SAFETY: sqlite3_vec_init is a valid SQLite extension entry point.
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }
}

/// Serialize a `&[f32]` to little-endian IEEE 754 bytes for sqlite-vec.
pub(crate) fn f32_to_le_bytes(v: &[f32]) -> &[u8] {
    bytemuck::cast_slice(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VecReadPool;

    #[test]
    fn open_and_upsert() {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::open(dir.path()).unwrap();
        let pool = VecReadPool::new(dir.path(), 2).unwrap();

        let entries = vec![
            EmbedEntry {
                id: 1,
                qualified_name: "crate::foo".into(),
                vector: vec![0.1_f32; VECTOR_DIM],
                body_hash: vec![0u8; 32],
                lang: "rust".into(),
            },
            EmbedEntry {
                id: 2,
                qualified_name: "crate::bar".into(),
                vector: vec![0.9_f32; VECTOR_DIM],
                body_hash: vec![1u8; 32],
                lang: "rust".into(),
            },
        ];
        store.upsert_embeddings(&entries).unwrap();

        let results = pool.search(vec![0.1_f32; VECTOR_DIM], None, 10).unwrap();
        assert!(!results.is_empty(), "should find entries");
        assert_eq!(results[0].0, 1, "closest to [0.1; 768] should be id=1");
    }

    #[test]
    fn search_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let _store = VectorStore::open(dir.path()).unwrap();
        let pool = VecReadPool::new(dir.path(), 2).unwrap();
        let results = pool.search(vec![0.1_f32; VECTOR_DIM], None, 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn lang_filter() {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::open(dir.path()).unwrap();
        let pool = VecReadPool::new(dir.path(), 2).unwrap();

        let entries = vec![
            EmbedEntry {
                id: 1,
                qualified_name: "rust_fn".into(),
                vector: vec![0.1_f32; VECTOR_DIM],
                body_hash: vec![0u8; 32],
                lang: "rust".into(),
            },
            EmbedEntry {
                id: 2,
                qualified_name: "py_fn".into(),
                vector: vec![0.1_f32; VECTOR_DIM],
                body_hash: vec![1u8; 32],
                lang: "python".into(),
            },
        ];
        store.upsert_embeddings(&entries).unwrap();

        let rust_only = pool
            .search(vec![0.1_f32; VECTOR_DIM], Some("rust"), 10)
            .unwrap();
        assert_eq!(rust_only.len(), 1);
        assert_eq!(rust_only[0].0, 1);
    }

    #[test]
    fn existing_hashes_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::open(dir.path()).unwrap();
        let pool = VecReadPool::new(dir.path(), 2).unwrap();

        let entries = vec![EmbedEntry {
            id: 42,
            qualified_name: "foo".into(),
            vector: vec![0.1_f32; VECTOR_DIM],
            body_hash: vec![0xABu8; 32],
            lang: "rust".into(),
        }];
        store.upsert_embeddings(&entries).unwrap();

        let hashes = pool.existing_hashes(&[42, 99]).unwrap();
        assert!(hashes.contains_key(&42));
        assert!(!hashes.contains_key(&99));
        assert_eq!(hashes[&42], [0xABu8; 32]);
    }
}
