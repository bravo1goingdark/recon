//! Lock-free read pool for the sqlite-vec vector store.
//!
//! Mirrors the `ReadPool` pattern from `recon-storage`: an
//! [`crossbeam_queue::ArrayQueue`] of read-only [`rusqlite::Connection`]s is
//! popped on entry, used, and pushed back on exit. If the queue is empty a
//! new overflow connection is opened on-the-fly. All reads are fully
//! concurrent — no mutex is ever held.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crossbeam_queue::ArrayQueue;
use rusqlite::{params, Connection, OpenFlags};

use crate::error::EmbedError;
use crate::vector_store::{f32_to_le_bytes, register_sqlite_vec};

/// Lock-free pool of read-only sqlite-vec connections.
///
/// Shared via `Arc` between the MCP tool handler and the watcher's hash-check
/// step. Operations never block each other.
pub struct VecReadPool {
    db_path: PathBuf,
    pool: ArrayQueue<Connection>,
}

impl VecReadPool {
    /// Open `capacity` read-only connections to `dir/embed.db`.
    ///
    /// The DB file must already exist (created by [`crate::VectorStore::open`]).
    pub fn new(dir: &Path, capacity: usize) -> Result<Self, EmbedError> {
        let db_path = dir.join("embed.db");
        let pool = ArrayQueue::new(capacity);
        for _ in 0..capacity {
            let conn = open_read_conn(&db_path)?;
            let _ = pool.push(conn); // queue has exactly `capacity` slots
        }
        Ok(Self { db_path, pool })
    }

    /// Borrow a connection, run `f`, return it. Lock-free.
    ///
    /// If the pool is empty (all connections in use) a new overflow connection
    /// is opened. On return the connection is pushed back; if the queue is
    /// already full (shouldn't happen under normal use) it is dropped.
    fn with<R>(
        &self,
        f: impl FnOnce(&Connection) -> Result<R, EmbedError>,
    ) -> Result<R, EmbedError> {
        let conn = match self.pool.pop() {
            Some(c) => c,
            None => open_read_conn(&self.db_path)?,
        };
        let result = f(&conn);
        let _ = self.pool.push(conn);
        result
    }

    /// Vector similarity search with optional language filter.
    ///
    /// Returns `(symbol_id, distance)` pairs sorted ascending by distance
    /// (lower = more similar).
    pub fn search(
        &self,
        query_vector: Vec<f32>,
        lang_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(u64, f32)>, EmbedError> {
        let query_bytes = f32_to_le_bytes(&query_vector);
        self.with(|conn| {
            if let Some(lang) = lang_filter {
                let mut stmt = conn
                    .prepare(
                        "SELECT s.rowid, s.distance \
                         FROM symbol_embeddings s \
                         WHERE s.embedding MATCH ?1 \
                           AND s.rowid IN (SELECT id FROM embed_meta WHERE lang = ?2) \
                         ORDER BY s.distance \
                         LIMIT ?3",
                    )
                    .map_err(|e| EmbedError::Store(format!("prepare: {e}")))?;
                let hits = stmt
                    .query_map(params![query_bytes, lang, limit as i64], |row| {
                        Ok((row.get::<_, i64>(0)? as u64, row.get::<_, f32>(1)?))
                    })
                    .map_err(|e| EmbedError::Store(format!("query: {e}")))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| EmbedError::Store(format!("row: {e}")))?;
                return Ok(hits);
            }

            let mut stmt = conn
                .prepare(
                    "SELECT rowid, distance \
                     FROM symbol_embeddings \
                     WHERE embedding MATCH ?1 \
                     ORDER BY distance \
                     LIMIT ?2",
                )
                .map_err(|e| EmbedError::Store(format!("prepare: {e}")))?;
            let hits = stmt
                .query_map(params![query_bytes, limit as i64], |row| {
                    Ok((row.get::<_, i64>(0)? as u64, row.get::<_, f32>(1)?))
                })
                .map_err(|e| EmbedError::Store(format!("query: {e}")))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| EmbedError::Store(format!("row: {e}")))?;
            Ok(hits)
        })
    }

    /// Return the stored `body_hash` for each of the given IDs.
    ///
    /// IDs absent from the store are omitted — the caller treats them as
    /// needing embedding. Used in the watcher to skip unchanged symbols.
    pub fn existing_hashes(&self, ids: &[u64]) -> Result<HashMap<u64, [u8; 32]>, EmbedError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        // Build IN list from plain integers — no user input, so no injection risk.
        let in_list = ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT id, body_hash FROM embed_meta WHERE id IN ({in_list})");
        self.with(|conn| {
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| EmbedError::Store(format!("prepare hashes: {e}")))?;
            let pairs = stmt
                .query_map([], |row| {
                    let id = row.get::<_, i64>(0)? as u64;
                    let hash: Vec<u8> = row.get(1)?;
                    Ok((id, hash))
                })
                .map_err(|e| EmbedError::Store(format!("query: {e}")))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| EmbedError::Store(format!("row: {e}")))?;
            let mut map = HashMap::with_capacity(pairs.len());
            for (id, hash) in pairs {
                if hash.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&hash);
                    map.insert(id, arr);
                }
            }
            Ok(map)
        })
    }
}

/// Open a single read-only connection with WAL and performance pragmas.
fn open_read_conn(db_path: &Path) -> Result<Connection, EmbedError> {
    register_sqlite_vec();
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| EmbedError::Store(format!("vec read pool open: {e}")))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA cache_size=-32000;
         PRAGMA mmap_size=268435456;
         PRAGMA temp_store=MEMORY;
         PRAGMA query_only=ON;",
    )
    .map_err(|e| EmbedError::Store(format!("vec read pool pragmas: {e}")))?;
    Ok(conn)
}
