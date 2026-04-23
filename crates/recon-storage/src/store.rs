//! SQLite-backed symbol store — optimized for batch inserts and cached queries.

use crate::schema;
use compact_str::CompactString;
use recon_core::error::Error;
use recon_core::lang::Language;
use recon_core::symbol::{FileMeta, Ref, Symbol, SymbolKind};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The main storage handle (single-writer).
pub struct Store {
    conn: Connection,
    /// Stored so `ReadPool` can open the same database.
    db_path: Option<PathBuf>,
}

impl Store {
    /// Open or create a store at the given path.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let conn = Connection::open(path).map_err(|e| Error::Storage(e.to_string()))?;
        Self::init(conn, Some(path.to_path_buf()))
    }

    /// Create an in-memory store (for testing).
    pub fn open_memory() -> Result<Self, Error> {
        let conn = Connection::open_in_memory().map_err(|e| Error::Storage(e.to_string()))?;
        Self::init(conn, None)
    }

    /// Get the database file path (None for in-memory stores).
    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    fn init(mut conn: Connection, db_path: Option<PathBuf>) -> Result<Self, Error> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA cache_size=-32000;
             PRAGMA mmap_size=268435456;
             PRAGMA temp_store=MEMORY;
             PRAGMA auto_vacuum=INCREMENTAL;",
        )
        .map_err(|e| Error::Storage(e.to_string()))?;

        schema::migrations()
            .to_latest(&mut conn)
            .map_err(|e| Error::Storage(e.to_string()))?;

        Ok(Self { conn, db_path })
    }

    /// Insert or update a file metadata record.
    pub fn upsert_file(&self, meta: &FileMeta) -> Result<(), Error> {
        self.conn
            .prepare_cached(
                "INSERT INTO files(path, lang, size_bytes, content_hash, mtime, indexed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(path) DO UPDATE SET
                    lang=excluded.lang, size_bytes=excluded.size_bytes,
                    content_hash=excluded.content_hash, mtime=excluded.mtime,
                    indexed_at=excluded.indexed_at",
            )
            .map_err(|e| Error::Storage(e.to_string()))?
            .execute(params![
                meta.path.to_str().unwrap_or(""),
                meta.lang.name(),
                meta.size_bytes as i64,
                meta.content_hash.as_slice(),
                meta.mtime,
                meta.indexed_at,
            ])
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Delete a file and cascade to its symbols and refs.
    ///
    /// Single transaction: delete refs (explicit), then file (FK cascades to symbols).
    pub fn delete_file_cascade(&self, path: &Path) -> Result<(), Error> {
        let path_str = path.to_str().unwrap_or("");
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| Error::Storage(e.to_string()))?;
        tx.execute(
            "DELETE FROM refs WHERE src_symbol_id IN (SELECT id FROM symbols WHERE path = ?1)",
            params![path_str],
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
        // FK cascade from files -> symbols handles symbol cleanup
        tx.execute("DELETE FROM files WHERE path = ?1", params![path_str])
            .map_err(|e| Error::Storage(e.to_string()))?;
        tx.commit().map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Insert a symbol, returning its assigned ID.
    /// Doc is stored separately in symbol_docs to reduce main table size.
    pub fn insert_symbol(&self, sym: &Symbol) -> Result<u64, Error> {
        self.conn
            .prepare_cached(
                "INSERT INTO symbols(path, name, qualified_name, kind, signature, parent_id,
                                      byte_start, byte_end, line_start, line_end, body_hash)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )
            .map_err(|e| Error::Storage(e.to_string()))?
            .execute(params![
                sym.path.to_str().unwrap_or(""),
                sym.name.as_str(),
                sym.qualified_name.as_str(),
                sym.kind.label(),
                sym.signature.as_deref(),
                sym.parent_id.map(|v| v as i64),
                sym.byte_range.start as i64,
                sym.byte_range.end as i64,
                *sym.line_range.start(),
                *sym.line_range.end(),
                sym.body_hash.as_slice(),
            ])
            .map_err(|e| Error::Storage(e.to_string()))?;
        let id = self.conn.last_insert_rowid() as u64;

        // Store doc separately if present
        if let Some(ref doc) = sym.doc {
            self.conn
                .prepare_cached(
                    "INSERT OR REPLACE INTO symbol_docs(symbol_id, doc) VALUES (?1, ?2)",
                )
                .map_err(|e| Error::Storage(e.to_string()))?
                .execute(params![id as i64, doc.as_str()])
                .map_err(|e| Error::Storage(e.to_string()))?;
        }

        Ok(id)
    }

    /// Batch-insert symbols in a single transaction.
    ///
    /// Uses a single prepared statement executed per symbol within one
    /// transaction — much faster than individual transactions.
    /// Doc is stored separately in symbol_docs.
    pub fn insert_symbols_batch(&self, symbols: &[Symbol]) -> Result<(), Error> {
        if symbols.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| Error::Storage(e.to_string()))?;
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO symbols(path, name, qualified_name, kind, signature, parent_id,
                                          byte_start, byte_end, line_start, line_end, body_hash)
                      VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut doc_stmt = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO symbol_docs(symbol_id, doc) VALUES (?1, ?2)",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;

            for sym in symbols {
                stmt.execute(params![
                    sym.path.to_str().unwrap_or(""),
                    sym.name.as_str(),
                    sym.qualified_name.as_str(),
                    sym.kind.label(),
                    sym.signature.as_deref(),
                    sym.parent_id.map(|v| v as i64),
                    sym.byte_range.start as i64,
                    sym.byte_range.end as i64,
                    *sym.line_range.start(),
                    *sym.line_range.end(),
                    sym.body_hash.as_slice(),
                ])
                .map_err(|e| Error::Storage(e.to_string()))?;

                if let Some(ref doc) = sym.doc {
                    let id = tx.last_insert_rowid();
                    doc_stmt
                        .execute(params![id, doc.as_str()])
                        .map_err(|e| Error::Storage(e.to_string()))?;
                }
            }
        }
        tx.commit().map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Batch-insert file metadata + symbols + refs in a single transaction.
    pub fn batch_index_file(
        &self,
        meta: &FileMeta,
        symbols: &[Symbol],
        refs: &[Ref],
    ) -> Result<(), Error> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| Error::Storage(e.to_string()))?;
        {
            let path_str = meta.path.to_str().unwrap_or("");

            // Delete old refs + symbols in one transaction
            tx.execute(
                "DELETE FROM refs WHERE src_symbol_id IN (SELECT id FROM symbols WHERE path = ?1)",
                params![path_str],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            tx.execute("DELETE FROM symbols WHERE path = ?1", params![path_str])
                .map_err(|e| Error::Storage(e.to_string()))?;

            // Upsert file
            tx.prepare_cached(
                "INSERT INTO files(path, lang, size_bytes, content_hash, mtime, indexed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(path) DO UPDATE SET
                    lang=excluded.lang, size_bytes=excluded.size_bytes,
                    content_hash=excluded.content_hash, mtime=excluded.mtime,
                    indexed_at=excluded.indexed_at",
            )
            .map_err(|e| Error::Storage(e.to_string()))?
            .execute(params![
                path_str,
                meta.lang.name(),
                meta.size_bytes as i64,
                meta.content_hash.as_slice(),
                meta.mtime,
                meta.indexed_at,
            ])
            .map_err(|e| Error::Storage(e.to_string()))?;

            // Batch symbols (doc stored separately)
            if !symbols.is_empty() {
                let mut sym_stmt = tx
                    .prepare_cached(
                        "INSERT INTO symbols(path, name, qualified_name, kind, signature, parent_id,
                                             byte_start, byte_end, line_start, line_end, body_hash)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    )
                    .map_err(|e| Error::Storage(e.to_string()))?;
                let mut doc_stmt = tx
                    .prepare_cached(
                        "INSERT OR REPLACE INTO symbol_docs(symbol_id, doc) VALUES (?1, ?2)",
                    )
                    .map_err(|e| Error::Storage(e.to_string()))?;

                for sym in symbols {
                    sym_stmt
                        .execute(params![
                            sym.path.to_str().unwrap_or(""),
                            sym.name.as_str(),
                            sym.qualified_name.as_str(),
                            sym.kind.label(),
                            sym.signature.as_deref(),
                            sym.parent_id.map(|v| v as i64),
                            sym.byte_range.start as i64,
                            sym.byte_range.end as i64,
                            *sym.line_range.start(),
                            *sym.line_range.end(),
                            sym.body_hash.as_slice(),
                        ])
                        .map_err(|e| Error::Storage(e.to_string()))?;

                    if let Some(ref doc) = sym.doc {
                        let id = tx.last_insert_rowid();
                        doc_stmt
                            .execute(params![id, doc.as_str()])
                            .map_err(|e| Error::Storage(e.to_string()))?;
                    }
                }
            }

            // Batch refs
            if !refs.is_empty() {
                let mut ref_stmt = tx
                    .prepare_cached(
                        "INSERT INTO refs(src_path, src_symbol_id, ident, dst_symbol_id, weight)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                    )
                    .map_err(|e| Error::Storage(e.to_string()))?;

                for r in refs {
                    ref_stmt
                        .execute(params![
                            r.src_path.to_str().unwrap_or(""),
                            r.src_symbol_id as i64,
                            r.ident.as_str(),
                            r.dst_symbol_id.map(|v| v as i64),
                            r.weight,
                        ])
                        .map_err(|e| Error::Storage(e.to_string()))?;
                }
            }
        }
        tx.commit().map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Batch-insert multiple files' metadata + symbols + refs in a single transaction.
    ///
    /// Much faster than calling `batch_index_file` per file — one BEGIN/COMMIT
    /// instead of N, and WAL syncs once instead of N times.
    pub fn batch_index_files(&self, files: &[(&FileMeta, &[Symbol], &[Ref])]) -> Result<(), Error> {
        if files.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| Error::Storage(e.to_string()))?;
        {
            let mut del_refs_stmt = tx
                .prepare_cached(
                    "DELETE FROM refs WHERE src_symbol_id IN (SELECT id FROM symbols WHERE path = ?1)",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut del_sym_stmt = tx
                .prepare_cached("DELETE FROM symbols WHERE path = ?1")
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut file_stmt = tx
                .prepare_cached(
                    "INSERT INTO files(path, lang, size_bytes, content_hash, mtime, indexed_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(path) DO UPDATE SET
                        lang=excluded.lang, size_bytes=excluded.size_bytes,
                        content_hash=excluded.content_hash, mtime=excluded.mtime,
                        indexed_at=excluded.indexed_at",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut sym_stmt = tx
                .prepare_cached(
                    "INSERT INTO symbols(path, name, qualified_name, kind, signature, parent_id,
                                         byte_start, byte_end, line_start, line_end, body_hash)
                      VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut doc_stmt = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO symbol_docs(symbol_id, doc) VALUES (?1, ?2)",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut ref_stmt = tx
                .prepare_cached(
                    "INSERT INTO refs(src_path, src_symbol_id, ident, dst_symbol_id, weight)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;

            for &(meta, symbols, refs) in files {
                let path_str = meta.path.to_str().unwrap_or("");
                del_refs_stmt
                    .execute(params![path_str])
                    .map_err(|e| Error::Storage(e.to_string()))?;
                del_sym_stmt
                    .execute(params![path_str])
                    .map_err(|e| Error::Storage(e.to_string()))?;
                file_stmt
                    .execute(params![
                        path_str,
                        meta.lang.name(),
                        meta.size_bytes as i64,
                        meta.content_hash.as_slice(),
                        meta.mtime,
                        meta.indexed_at,
                    ])
                    .map_err(|e| Error::Storage(e.to_string()))?;
                for sym in symbols {
                    sym_stmt
                        .execute(params![
                            sym.path.to_str().unwrap_or(""),
                            sym.name.as_str(),
                            sym.qualified_name.as_str(),
                            sym.kind.label(),
                            sym.signature.as_deref(),
                            sym.parent_id.map(|v| v as i64),
                            sym.byte_range.start as i64,
                            sym.byte_range.end as i64,
                            *sym.line_range.start(),
                            *sym.line_range.end(),
                            sym.body_hash.as_slice(),
                        ])
                        .map_err(|e| Error::Storage(e.to_string()))?;

                    if let Some(ref doc) = sym.doc {
                        let id = tx.last_insert_rowid();
                        doc_stmt
                            .execute(params![id, doc.as_str()])
                            .map_err(|e| Error::Storage(e.to_string()))?;
                    }
                }
                for r in refs {
                    ref_stmt
                        .execute(params![
                            r.src_path.to_str().unwrap_or(""),
                            r.src_symbol_id as i64,
                            r.ident.as_str(),
                            r.dst_symbol_id.map(|v| v as i64),
                            r.weight,
                        ])
                        .map_err(|e| Error::Storage(e.to_string()))?;
                }
            }
        }
        tx.commit().map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Delete all symbols (and cascaded refs) for a given file path.
    pub fn delete_symbols_for_path(&self, path: &Path) -> Result<(), Error> {
        let path_str = path.to_str().unwrap_or("");
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| Error::Storage(e.to_string()))?;
        // Delete refs explicitly (no FK cascade), then symbols
        tx.execute(
            "DELETE FROM refs WHERE src_symbol_id IN (SELECT id FROM symbols WHERE path = ?1)",
            params![path_str],
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
        tx.execute("DELETE FROM symbols WHERE path = ?1", params![path_str])
            .map_err(|e| Error::Storage(e.to_string()))?;
        tx.commit().map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Look up a symbol by qualified name.
    pub fn get_symbol_by_qname(&self, qname: &str) -> Result<Option<Symbol>, Error> {
        self.conn
            .query_row(
                "SELECT s.id, s.path, s.name, s.qualified_name, s.kind, s.signature,
                        sd.doc, s.parent_id,
                        s.byte_start, s.byte_end, s.line_start, s.line_end, s.body_hash
                 FROM symbols s
                 LEFT JOIN symbol_docs sd ON sd.symbol_id = s.id
                 WHERE s.qualified_name = ?1 COLLATE NOCASE",
                params![qname],
                |row| Ok(row_to_symbol(row)),
            )
            .optional()
            .map_err(|e| Error::Storage(e.to_string()))?
            .transpose()
    }

    /// Find symbols by exact name (case-insensitive).
    pub fn find_symbols_exact(&self, name: &str, limit: usize) -> Result<Vec<Symbol>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT s.id, s.path, s.name, s.qualified_name, s.kind, s.signature,
                        sd.doc, s.parent_id,
                        s.byte_start, s.byte_end, s.line_start, s.line_end, s.body_hash
                 FROM symbols s
                 LEFT JOIN symbol_docs sd ON sd.symbol_id = s.id
                 WHERE s.name = ?1 COLLATE NOCASE LIMIT ?2",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

        let rows = stmt
            .query_map(params![name, limit as i64], |row| Ok(row_to_symbol(row)))
            .map_err(|e| Error::Storage(e.to_string()))?;

        let mut results = Vec::with_capacity(limit.min(64));
        for r in rows {
            results.push(r.map_err(|e| Error::Storage(e.to_string()))??);
        }
        Ok(results)
    }

    /// Fuzzy search symbols via FTS5 trigram.
    pub fn search_symbols_fuzzy(&self, query: &str, limit: usize) -> Result<Vec<Symbol>, Error> {
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT s.id, s.path, s.name, s.qualified_name, s.kind, s.signature,
                        sd.doc, s.parent_id,
                        s.byte_start, s.byte_end, s.line_start, s.line_end, s.body_hash
                 FROM symbols_fts f
                 JOIN symbols s ON f.rowid = s.id
                 LEFT JOIN symbol_docs sd ON sd.symbol_id = s.id
                 WHERE symbols_fts MATCH ?1
                 LIMIT ?2",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

        let rows = stmt
            .query_map(params![query, limit as i64], |row| Ok(row_to_symbol(row)))
            .map_err(|e| Error::Storage(e.to_string()))?;

        let mut results = Vec::with_capacity(limit.min(64));
        for r in rows {
            results.push(r.map_err(|e| Error::Storage(e.to_string()))??);
        }
        Ok(results)
    }

    /// Insert a batch of refs.
    pub fn insert_refs(&self, refs: &[Ref]) -> Result<(), Error> {
        if refs.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| Error::Storage(e.to_string()))?;
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO refs(src_path, src_symbol_id, ident, dst_symbol_id, weight)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;

            for r in refs {
                stmt.execute(params![
                    r.src_path.to_str().unwrap_or(""),
                    r.src_symbol_id as i64,
                    r.ident.as_str(),
                    r.dst_symbol_id.map(|v| v as i64),
                    r.weight,
                ])
                .map_err(|e| Error::Storage(e.to_string()))?;
            }
        }
        tx.commit().map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Find all refs for a given identifier.
    pub fn refs_for_ident(&self, ident: &str) -> Result<Vec<Ref>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT src_path, src_symbol_id, ident, dst_symbol_id, weight
                 FROM refs WHERE ident = ?1",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

        let rows = stmt
            .query_map(params![ident], |row| {
                Ok(Ref {
                    src_path: Arc::new(PathBuf::from(row.get::<_, String>(0)?)),
                    src_symbol_id: row.get::<_, i64>(1)? as u64,
                    ident: CompactString::new(row.get::<_, String>(2)?),
                    dst_symbol_id: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                    weight: row.get(4)?,
                })
            })
            .map_err(|e| Error::Storage(e.to_string()))?;

        let mut results = Vec::with_capacity(32);
        for r in rows {
            results.push(r.map_err(|e| Error::Storage(e.to_string()))?);
        }
        Ok(results)
    }

    /// Get file content hash (returns None if file not indexed).
    pub fn get_file_hash(&self, path: &Path) -> Result<Option<[u8; 32]>, Error> {
        let path_str = path.to_str().unwrap_or("");
        self.conn
            .query_row(
                "SELECT content_hash FROM files WHERE path = ?1",
                params![path_str],
                |row| {
                    let blob = row.get_ref(0)?;
                    let bytes = blob.as_blob().map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            0,
                            "content_hash".into(),
                            rusqlite::types::Type::Blob,
                        )
                    })?;
                    if bytes.len() != 32 {
                        return Err(rusqlite::Error::InvalidColumnType(
                            0,
                            "content_hash".into(),
                            rusqlite::types::Type::Blob,
                        ));
                    }
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(bytes);
                    Ok(hash)
                },
            )
            .optional()
            .map_err(|e| Error::Storage(e.to_string()))
    }

    /// Get or set a meta key.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>, Error> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| Error::Storage(e.to_string()))
    }

    /// Set a meta key.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, value],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Count all symbols in the store.
    pub fn symbol_count(&self) -> Result<u64, Error> {
        self.conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|n| n as u64)
            .map_err(|e| Error::Storage(e.to_string()))
    }

    /// Most recent `indexed_at` across all files — changes on any reindex.
    pub fn max_indexed_at(&self) -> Result<i64, Error> {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(indexed_at), 0) FROM files",
                [],
                |row| row.get(0),
            )
            .map_err(|e| Error::Storage(e.to_string()))
    }

    /// Get symbol counts and top-3 symbol names per file in a single query.
    ///
    /// Returns `(path, symbol_count, top_symbols)` tuples. Much faster than
    /// calling `symbols_for_path` per file.
    pub fn file_symbol_summaries(
        &self,
    ) -> Result<Vec<(PathBuf, usize, Vec<CompactString>)>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT f.path,
                        COUNT(s.id) AS cnt,
                        GROUP_CONCAT(
                            CASE WHEN s.parent_id IS NULL
                                 THEN s.kind || ' ' || s.name
                                 ELSE NULL
                            END, '|'
                        ) AS top
                 FROM files f
                 LEFT JOIN symbols s ON s.path = f.path
                 GROUP BY f.path
                 ORDER BY f.path",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                let path: String = row.get(0)?;
                let count: usize = row.get::<_, i64>(1)? as usize;
                let top_raw: Option<String> = row.get(2)?;
                let top: Vec<CompactString> = top_raw
                    .unwrap_or_default()
                    .split('|')
                    .filter(|s| !s.is_empty())
                    .take(3)
                    .map(CompactString::from)
                    .collect();
                Ok((PathBuf::from(path), count, top))
            })
            .map_err(|e| Error::Storage(e.to_string()))?;

        let mut results = Vec::with_capacity(128);
        for r in rows {
            results.push(r.map_err(|e| Error::Storage(e.to_string()))?);
        }
        Ok(results)
    }

    /// Delete all meta entries whose key starts with a given prefix.
    pub fn delete_meta_prefix(&self, prefix: &str) -> Result<(), Error> {
        self.conn
            .execute(
                "DELETE FROM meta WHERE key LIKE ?1",
                params![format!("{prefix}%")],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// List all symbols for a path.
    pub fn symbols_for_path(&self, path: &Path) -> Result<Vec<Symbol>, Error> {
        let path_str = path.to_str().unwrap_or("");
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT s.id, s.path, s.name, s.qualified_name, s.kind, s.signature,
                        sd.doc, s.parent_id,
                        s.byte_start, s.byte_end, s.line_start, s.line_end, s.body_hash
                 FROM symbols s
                 LEFT JOIN symbol_docs sd ON sd.symbol_id = s.id
                 WHERE s.path = ?1 ORDER BY s.byte_start",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

        let rows = stmt
            .query_map(params![path_str], |row| Ok(row_to_symbol(row)))
            .map_err(|e| Error::Storage(e.to_string()))?;

        let mut results = Vec::with_capacity(32);
        for r in rows {
            results.push(r.map_err(|e| Error::Storage(e.to_string()))??);
        }
        Ok(results)
    }

    /// Load all refs in a single query (for bulk operations like PageRank).
    ///
    /// Dedups `Arc<PathBuf>` across rows sharing the same `src_path` —
    /// saves ~80% of path allocations on typical repos (most refs cluster
    /// into a few source files).
    pub fn all_refs(&self) -> Result<Vec<Ref>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT src_path, src_symbol_id, ident, dst_symbol_id, weight FROM refs",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

        let mut path_interner: std::collections::HashMap<String, Arc<PathBuf>> =
            std::collections::HashMap::with_capacity(2048);

        let rows = stmt
            .query_map([], |row| {
                let path_str: String = row.get(0)?;
                let src_path = path_interner
                    .entry(path_str)
                    .or_insert_with_key(|k| Arc::new(PathBuf::from(k.as_str())))
                    .clone();
                Ok(Ref {
                    src_path,
                    src_symbol_id: row.get::<_, i64>(1)? as u64,
                    ident: CompactString::new(row.get::<_, String>(2)?),
                    dst_symbol_id: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                    weight: row.get(4)?,
                })
            })
            .map_err(|e| Error::Storage(e.to_string()))?;

        let mut results = Vec::with_capacity(1024);
        for r in rows {
            results.push(r.map_err(|e| Error::Storage(e.to_string()))?);
        }
        Ok(results)
    }

    /// Load all symbols in a single query (for bulk operations like PageRank).
    ///
    /// Interns `Arc<PathBuf>` across rows sharing the same `path` to avoid
    /// ~78 K redundant `PathBuf` allocations on an 80 K-symbol repo.
    pub fn all_symbols(&self) -> Result<Vec<Symbol>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT s.id, s.path, s.name, s.qualified_name, s.kind, s.signature,
                        sd.doc, s.parent_id,
                        s.byte_start, s.byte_end, s.line_start, s.line_end, s.body_hash
                 FROM symbols s
                 LEFT JOIN symbol_docs sd ON sd.symbol_id = s.id
                 ORDER BY s.path, s.byte_start",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

        let mut path_interner: std::collections::HashMap<String, Arc<PathBuf>> =
            std::collections::HashMap::with_capacity(2048);

        let rows = stmt
            .query_map([], |row| {
                Ok(row_to_symbol_interned(row, &mut path_interner))
            })
            .map_err(|e| Error::Storage(e.to_string()))?;

        let mut results = Vec::with_capacity(1024);
        for r in rows {
            results.push(r.map_err(|e| Error::Storage(e.to_string()))??);
        }
        Ok(results)
    }

    /// List all indexed file paths.
    pub fn all_file_paths(&self) -> Result<Vec<PathBuf>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT path FROM files ORDER BY path")
            .map_err(|e| Error::Storage(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                let p: String = row.get(0)?;
                Ok(PathBuf::from(p))
            })
            .map_err(|e| Error::Storage(e.to_string()))?;

        let mut results = Vec::with_capacity(128);
        for r in rows {
            results.push(r.map_err(|e| Error::Storage(e.to_string()))?);
        }
        Ok(results)
    }

    /// Get file paths filtered by language — pushes filter into SQL.
    pub fn file_paths_by_lang(&self, lang: &str) -> Result<Vec<PathBuf>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT path FROM files WHERE lang = ?1")
            .map_err(|e| Error::Storage(e.to_string()))?;

        let rows = stmt
            .query_map(params![lang], |row| {
                let p: String = row.get(0)?;
                Ok(PathBuf::from(p))
            })
            .map_err(|e| Error::Storage(e.to_string()))?;

        let mut results = Vec::with_capacity(64);
        for r in rows {
            results.push(r.map_err(|e| Error::Storage(e.to_string()))?);
        }
        Ok(results)
    }

    /// Rebuild the FTS5 index from the content table.
    ///
    /// Call this after bulk inserts with triggers disabled, or to repair
    /// a corrupted FTS index.
    pub fn rebuild_fts(&self) -> Result<(), Error> {
        self.conn
            .execute_batch("INSERT INTO symbols_fts(symbols_fts) VALUES('rebuild');")
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Run VACUUM to reclaim unused space and defragment the database.
    ///
    /// Call after bulk deletes or large batch inserts to shrink the file.
    /// This is a blocking operation — run it during idle periods.
    pub fn vacuum(&self) -> Result<(), Error> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Run incremental vacuum to reclaim free pages without the full VACUUM cost.
    pub fn incremental_vacuum(&self) -> Result<(), Error> {
        self.conn
            .execute_batch("PRAGMA incremental_vacuum; PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Get the docstring for a symbol from the separate symbol_docs table.
    /// Returns None if no doc is stored (docs are not stored in the main symbols table).
    pub fn get_symbol_doc(&self, symbol_id: u64) -> Result<Option<String>, Error> {
        self.conn
            .query_row(
                "SELECT doc FROM symbol_docs WHERE symbol_id = ?1",
                params![symbol_id as i64],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| Error::Storage(e.to_string()))
    }

    /// Enter high-throughput indexing mode: disable synchronous writes,
    /// increase cache size, and defer WAL checkpoints.
    ///
    /// Call `exit_indexing_mode()` after bulk indexing to restore safety.
    /// This can speed up bulk inserts by 2-3× at the cost of crash safety
    /// during the indexing window.
    pub fn enter_indexing_mode(&self) -> Result<(), Error> {
        self.conn
            .execute_batch(
                "PRAGMA synchronous=OFF;
                 PRAGMA cache_size=-64000;
                 PRAGMA wal_autocheckpoint=0;",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Exit high-throughput indexing mode and restore safe defaults.
    /// Also performs a WAL checkpoint to flush pending writes.
    pub fn exit_indexing_mode(&self) -> Result<(), Error> {
        self.conn
            .execute_batch(
                "PRAGMA wal_autocheckpoint=1000;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA cache_size=-32000;
                 PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.conn.execute_batch("PRAGMA optimize;");
    }
}

/// Convert a rusqlite row to a Symbol. Public so `read_fns` can reuse it.
/// Note: doc is no longer stored in the symbols table — it lives in symbol_docs
/// and is loaded separately via `get_symbol_doc`.
pub fn row_to_symbol(row: &rusqlite::Row<'_>) -> Result<Symbol, Error> {
    row_to_symbol_with_path_arc(row, None)
}

/// Bulk variant of [`row_to_symbol`] that reuses `Arc<PathBuf>` across rows
/// sharing the same path string. On a repo with 80 K symbols across 1.8 K
/// files (~45 symbols/file), this saves ~78 K `PathBuf` + `Arc` allocations
/// per call to `all_symbols`. Pass a per-query `HashMap` — the caller
/// owns its lifetime.
pub fn row_to_symbol_interned(
    row: &rusqlite::Row<'_>,
    interner: &mut std::collections::HashMap<String, Arc<PathBuf>>,
) -> Result<Symbol, Error> {
    row_to_symbol_with_path_arc(row, Some(interner))
}

fn row_to_symbol_with_path_arc(
    row: &rusqlite::Row<'_>,
    interner: Option<&mut std::collections::HashMap<String, Arc<PathBuf>>>,
) -> Result<Symbol, Error> {
    let kind = kind_from_row(row)?;

    let body_hash: [u8; 32] = {
        let blob = row.get_ref(12).map_err(|e| Error::Storage(e.to_string()))?;
        let bytes = blob
            .as_blob()
            .map_err(|_| Error::Storage("body_hash not a blob".into()))?;
        if bytes.len() != 32 {
            return Err(Error::Storage(format!(
                "invalid body_hash length: {}",
                bytes.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(bytes);
        arr
    };

    let path_str: String = row.get(1).map_err(|e| Error::Storage(e.to_string()))?;
    let lang = Language::from_path(Path::new(&path_str));
    let path = match interner {
        Some(map) => map
            .entry(path_str)
            .or_insert_with_key(|k| Arc::new(PathBuf::from(k.as_str())))
            .clone(),
        None => Arc::new(PathBuf::from(path_str)),
    };
    let byte_start: usize = row
        .get::<_, i64>(8)
        .map_err(|e| Error::Storage(e.to_string()))? as usize;
    let byte_end: usize = row
        .get::<_, i64>(9)
        .map_err(|e| Error::Storage(e.to_string()))? as usize;
    let line_start: u32 = row.get(10).map_err(|e| Error::Storage(e.to_string()))?;
    let line_end: u32 = row.get(11).map_err(|e| Error::Storage(e.to_string()))?;

    Ok(Symbol {
        id: row
            .get::<_, i64>(0)
            .map_err(|e| Error::Storage(e.to_string()))? as u64,
        path,
        name: CompactString::new(
            row.get::<_, String>(2)
                .map_err(|e| Error::Storage(e.to_string()))?,
        ),
        qualified_name: CompactString::new(
            row.get::<_, String>(3)
                .map_err(|e| Error::Storage(e.to_string()))?,
        ),
        kind,
        signature: row
            .get::<_, Option<String>>(5)
            .map_err(|e| Error::Storage(e.to_string()))?
            .map(CompactString::from),
        doc: row
            .get::<_, Option<String>>(6)
            .map_err(|e| Error::Storage(e.to_string()))?
            .map(CompactString::from),
        parent_id: row
            .get::<_, Option<i64>>(7)
            .map_err(|e| Error::Storage(e.to_string()))?
            .map(|v| v as u64),
        byte_range: byte_start..byte_end,
        line_range: line_start..=line_end,
        body_hash,
        lang,
    })
}

fn kind_from_row(row: &rusqlite::Row<'_>) -> Result<SymbolKind, Error> {
    let kind_str = row
        .get_ref(4)
        .map_err(|e| Error::Storage(e.to_string()))?
        .as_str()
        .map_err(|_| Error::Storage("kind column not text".into()))?;
    Ok(match kind_str {
        "fn" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        "struct" => SymbolKind::Struct,
        "class" => SymbolKind::Class,
        "interface" => SymbolKind::Interface,
        "enum" => SymbolKind::Enum,
        "variant" => SymbolKind::EnumVariant,
        "trait" => SymbolKind::Trait,
        "const" => SymbolKind::Const,
        "static" => SymbolKind::Static,
        "type" => SymbolKind::Type,
        "mod" => SymbolKind::Module,
        "macro" => SymbolKind::Macro,
        "field" => SymbolKind::Field,
        other => return Err(Error::Storage(format!("unknown symbol kind: {other}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use recon_core::lang::Language;

    fn make_symbol(name: &str, qname: &str, kind: SymbolKind) -> Symbol {
        Symbol {
            id: 0,
            path: Arc::new(PathBuf::from("src/lib.rs")),
            name: CompactString::new(name),
            qualified_name: CompactString::new(qname),
            kind,
            signature: Some(format!("fn {name}()").into()),
            doc: Some(format!("Docs for {name}").into()),
            parent_id: None,
            byte_range: 0..100,
            line_range: 1..=10,
            body_hash: crate::hash::blake3_bytes(name.as_bytes()),
            lang: Language::Rust,
        }
    }

    fn make_file_meta(path: &str) -> FileMeta {
        FileMeta {
            path: PathBuf::from(path),
            lang: Language::Rust,
            size_bytes: 1024,
            content_hash: [0u8; 32],
            mtime: 1000,
            indexed_at: 1001,
        }
    }

    #[test]
    fn open_memory_and_migrate() {
        let store = Store::open_memory().unwrap();
        let v = store.get_meta("schema_version").unwrap();
        assert_eq!(v.as_deref(), Some("4"));
    }

    #[test]
    fn upsert_and_get_file() {
        let store = Store::open_memory().unwrap();
        let meta = make_file_meta("src/lib.rs");
        store.upsert_file(&meta).unwrap();
        let hash = store.get_file_hash(Path::new("src/lib.rs")).unwrap();
        assert!(hash.is_some());
    }

    #[test]
    fn insert_symbol_and_find() {
        let store = Store::open_memory().unwrap();
        store.upsert_file(&make_file_meta("src/lib.rs")).unwrap();

        let sym = make_symbol(
            "validate_email",
            "mymod::validate_email",
            SymbolKind::Function,
        );
        let id = store.insert_symbol(&sym).unwrap();
        assert!(id > 0);

        let found = store
            .get_symbol_by_qname("mymod::validate_email")
            .unwrap()
            .unwrap();
        assert_eq!(found.name.as_str(), "validate_email");
    }

    #[test]
    fn find_exact() {
        let store = Store::open_memory().unwrap();
        store.upsert_file(&make_file_meta("src/lib.rs")).unwrap();
        store
            .insert_symbol(&make_symbol("Foo", "mymod::Foo", SymbolKind::Struct))
            .unwrap();
        store
            .insert_symbol(&make_symbol("foo", "mymod::foo", SymbolKind::Function))
            .unwrap();

        let results = store.find_symbols_exact("foo", 10).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn fuzzy_search() {
        let store = Store::open_memory().unwrap();
        store.upsert_file(&make_file_meta("src/lib.rs")).unwrap();
        store
            .insert_symbol(&make_symbol(
                "validate_email",
                "mymod::validate_email",
                SymbolKind::Function,
            ))
            .unwrap();

        let results = store.search_symbols_fuzzy("validate", 10).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].name.as_str(), "validate_email");
    }

    #[test]
    fn cascade_delete() {
        let store = Store::open_memory().unwrap();
        store.upsert_file(&make_file_meta("src/lib.rs")).unwrap();
        store
            .insert_symbol(&make_symbol("bar", "mymod::bar", SymbolKind::Function))
            .unwrap();

        assert_eq!(store.symbol_count().unwrap(), 1);
        store.delete_file_cascade(Path::new("src/lib.rs")).unwrap();
        assert_eq!(store.symbol_count().unwrap(), 0);
    }

    #[test]
    fn refs_roundtrip() {
        let store = Store::open_memory().unwrap();
        store.upsert_file(&make_file_meta("src/lib.rs")).unwrap();
        let id = store
            .insert_symbol(&make_symbol("foo", "mymod::foo", SymbolKind::Function))
            .unwrap();

        let refs = vec![Ref {
            src_path: Arc::new(PathBuf::from("src/main.rs")),
            src_symbol_id: id,
            ident: CompactString::new("foo"),
            dst_symbol_id: Some(id),
            weight: 1.0,
        }];
        store.insert_refs(&refs).unwrap();

        let found = store.refs_for_ident("foo").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].ident.as_str(), "foo");
    }

    #[test]
    fn bulk_insert_performance() {
        let store = Store::open_memory().unwrap();
        store.upsert_file(&make_file_meta("src/lib.rs")).unwrap();

        let symbols: Vec<Symbol> = (0..10_000)
            .map(|i| {
                let name = format!("sym_{i}");
                let qname = format!("mod::sym_{i}");
                make_symbol(&name, &qname, SymbolKind::Function)
            })
            .collect();

        let start = std::time::Instant::now();
        store.insert_symbols_batch(&symbols).unwrap();
        let elapsed = start.elapsed();
        eprintln!("10K batched symbol inserts took {elapsed:?}");
        assert!(
            elapsed.as_millis() < 2000,
            "10K inserts took too long: {elapsed:?}"
        );
        assert_eq!(store.symbol_count().unwrap(), 10_000);
    }

    #[test]
    fn meta_roundtrip() {
        let store = Store::open_memory().unwrap();
        store.set_meta("last_commit", "abc123").unwrap();
        assert_eq!(
            store.get_meta("last_commit").unwrap().as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn batch_index_file_works() {
        let store = Store::open_memory().unwrap();
        let meta = make_file_meta("src/lib.rs");
        let symbols = vec![
            make_symbol("foo", "mod::foo", SymbolKind::Function),
            make_symbol("bar", "mod::bar", SymbolKind::Function),
        ];
        let refs = vec![Ref {
            src_path: Arc::new(PathBuf::from("src/lib.rs")),
            src_symbol_id: 1,
            ident: CompactString::new("bar"),
            dst_symbol_id: None,
            weight: 1.0,
        }];

        store.batch_index_file(&meta, &symbols, &refs).unwrap();
        assert_eq!(store.symbol_count().unwrap(), 2);
        assert_eq!(store.refs_for_ident("bar").unwrap().len(), 1);
    }

    #[test]
    fn fk_cascade_deletes_refs_with_symbols() {
        let store = Store::open_memory().unwrap();
        let meta = make_file_meta("src/lib.rs");
        let symbols = vec![make_symbol("foo", "mod::foo", SymbolKind::Function)];
        let refs = vec![Ref {
            src_path: Arc::new(PathBuf::from("src/lib.rs")),
            src_symbol_id: 1,
            ident: CompactString::new("bar"),
            dst_symbol_id: None,
            weight: 1.0,
        }];

        store.batch_index_file(&meta, &symbols, &refs).unwrap();
        assert_eq!(store.refs_for_ident("bar").unwrap().len(), 1);

        // Deleting symbols should cascade-delete refs via FK
        store
            .delete_symbols_for_path(std::path::Path::new("src/lib.rs"))
            .unwrap();
        assert_eq!(store.refs_for_ident("bar").unwrap().len(), 0);
    }

    #[test]
    fn file_cascade_delete_removes_everything() {
        let store = Store::open_memory().unwrap();
        let meta = make_file_meta("src/lib.rs");
        let symbols = vec![make_symbol("foo", "mod::foo", SymbolKind::Function)];
        let refs = vec![Ref {
            src_path: Arc::new(PathBuf::from("src/lib.rs")),
            src_symbol_id: 1,
            ident: CompactString::new("baz"),
            dst_symbol_id: None,
            weight: 1.0,
        }];

        store.batch_index_file(&meta, &symbols, &refs).unwrap();
        assert_eq!(store.symbol_count().unwrap(), 1);
        assert_eq!(store.refs_for_ident("baz").unwrap().len(), 1);

        // Deleting the file should cascade to symbols and refs
        store
            .delete_file_cascade(std::path::Path::new("src/lib.rs"))
            .unwrap();
        assert_eq!(store.symbol_count().unwrap(), 0);
        assert_eq!(store.refs_for_ident("baz").unwrap().len(), 0);
    }

    #[test]
    fn rebuild_fts_works() {
        let store = Store::open_memory().unwrap();
        store.upsert_file(&make_file_meta("src/lib.rs")).unwrap();
        let sym = make_symbol(
            "validate_email",
            "mod::validate_email",
            SymbolKind::Function,
        );
        store.insert_symbol(&sym).unwrap();

        // FTS should find it via trigram
        let results = store.search_symbols_fuzzy("valid", 10).unwrap();
        assert!(!results.is_empty());

        // Rebuild FTS and verify still works
        store.rebuild_fts().unwrap();
        let results2 = store.search_symbols_fuzzy("valid", 10).unwrap();
        assert!(!results2.is_empty());
    }

    #[test]
    fn file_paths_by_lang_works() {
        let store = Store::open_memory().unwrap();
        store.upsert_file(&make_file_meta("src/lib.rs")).unwrap();
        let mut py_meta = make_file_meta("src/main.py");
        py_meta.lang = Language::Python;
        store.upsert_file(&py_meta).unwrap();

        let rust_files = store.file_paths_by_lang("Rust").unwrap();
        assert_eq!(rust_files.len(), 1);

        let py_files = store.file_paths_by_lang("Python").unwrap();
        assert_eq!(py_files.len(), 1);

        let go_files = store.file_paths_by_lang("Go").unwrap();
        assert!(go_files.is_empty());
    }

    #[test]
    fn file_symbol_summaries_compact_string() {
        let store = Store::open_memory().unwrap();
        store.upsert_file(&make_file_meta("src/lib.rs")).unwrap();

        let foo = make_symbol("foo", "foo", SymbolKind::Function);
        let bar = make_symbol("Bar", "Bar", SymbolKind::Struct);
        store.insert_symbol(&foo).unwrap();
        store.insert_symbol(&bar).unwrap();

        let summaries = store.file_symbol_summaries().unwrap();
        assert_eq!(summaries.len(), 1);
        let (path, count, top_syms) = &summaries[0];
        assert_eq!(path.to_string_lossy(), "src/lib.rs");
        assert_eq!(*count, 2);
        // top_syms must be CompactString (compile-time check) and contain kind + name
        assert!(top_syms
            .iter()
            .any(|s| s.contains("foo") || s.contains("Bar")));
    }

    #[test]
    fn file_symbol_summaries_empty_file() {
        let store = Store::open_memory().unwrap();
        store.upsert_file(&make_file_meta("src/empty.rs")).unwrap();
        let summaries = store.file_symbol_summaries().unwrap();
        assert_eq!(summaries.len(), 1);
        let (_, count, top_syms) = &summaries[0];
        assert_eq!(*count, 0);
        assert!(top_syms.is_empty());
    }

    #[test]
    fn row_to_symbol_compact_string_fields() {
        let store = Store::open_memory().unwrap();
        store.upsert_file(&make_file_meta("src/lib.rs")).unwrap();

        let sym = make_symbol("check", "mod::check", SymbolKind::Function);
        store.insert_symbol(&sym).unwrap();

        let found = store.find_symbols_exact("check", 1).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0]
            .signature
            .as_deref()
            .is_some_and(|s| s.contains("check")));
        // doc must be populated on the Symbol itself, not just via get_symbol_doc
        assert!(
            found[0].doc.is_some(),
            "doc should be populated via LEFT JOIN on symbol_docs"
        );
    }

    #[test]
    fn doc_roundtrip_via_fuzzy_search() {
        let store = Store::open_memory().unwrap();
        store.upsert_file(&make_file_meta("src/lib.rs")).unwrap();

        let sym = make_symbol("send_request", "mod::send_request", SymbolKind::Function);
        store.insert_symbol(&sym).unwrap();

        let results = store.search_symbols_fuzzy("send_request", 10).unwrap();
        assert!(!results.is_empty());
        assert!(
            results[0].doc.is_some(),
            "doc should be populated via fuzzy search LEFT JOIN"
        );
    }
}
