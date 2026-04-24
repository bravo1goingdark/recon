//! Free functions for read-only SQLite queries.
//!
//! These are extracted from `Store` methods so they can be shared between
//! the single write `Store` and the lock-free `ReadPool`. Each function
//! takes a `&Connection` — the caller is responsible for providing one.

use compact_str::CompactString;
use recon_core::error::Error;
use recon_core::symbol::{Ref, Symbol};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::store::{path_key, row_to_symbol, row_to_symbol_interned};

/// List all symbols for a path, ordered by byte offset.
pub fn symbols_for_path(conn: &Connection, path: &Path) -> Result<Vec<Symbol>, Error> {
    let path_str = path_key(path);
    let mut stmt = conn
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

/// Find symbols by exact name (case-insensitive).
pub fn find_symbols_exact(
    conn: &Connection,
    name: &str,
    limit: usize,
) -> Result<Vec<Symbol>, Error> {
    let mut stmt = conn
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
pub fn search_symbols_fuzzy(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<Symbol>, Error> {
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let mut stmt = conn
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

/// Look up a symbol by qualified name.
pub fn get_symbol_by_qname(conn: &Connection, qname: &str) -> Result<Option<Symbol>, Error> {
    conn.query_row(
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

/// Look up a symbol by its numeric ID.
pub fn symbol_by_id(conn: &Connection, id: u64) -> Result<Option<Symbol>, Error> {
    conn.query_row(
        "SELECT s.id, s.path, s.name, s.qualified_name, s.kind, s.signature,
                sd.doc, s.parent_id,
                s.byte_start, s.byte_end, s.line_start, s.line_end, s.body_hash
         FROM symbols s
         LEFT JOIN symbol_docs sd ON sd.symbol_id = s.id
         WHERE s.id = ?1",
        params![id as i64],
        |row| Ok(row_to_symbol(row)),
    )
    .optional()
    .map_err(|e| Error::Storage(e.to_string()))?
    .transpose()
}

/// Get the docstring for a symbol from the separate symbol_docs table.
pub fn symbol_doc_by_id(conn: &Connection, id: u64) -> Result<Option<String>, Error> {
    conn.query_row(
        "SELECT doc FROM symbol_docs WHERE symbol_id = ?1",
        params![id as i64],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| Error::Storage(e.to_string()))
}

/// Find all refs for a given identifier.
pub fn refs_for_ident(conn: &Connection, ident: &str) -> Result<Vec<Ref>, Error> {
    let mut stmt = conn
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
pub fn get_file_hash(conn: &Connection, path: &Path) -> Result<Option<[u8; 32]>, Error> {
    let path_str = path_key(path);
    conn.query_row(
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

/// Get a meta key.
pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>, Error> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| Error::Storage(e.to_string()))
}

/// Count all symbols.
pub fn symbol_count(conn: &Connection) -> Result<u64, Error> {
    conn.query_row("SELECT COUNT(*) FROM symbols", [], |row| {
        row.get::<_, i64>(0)
    })
    .map(|n| n as u64)
    .map_err(|e| Error::Storage(e.to_string()))
}

/// Count all indexed files.
pub fn file_count(conn: &Connection) -> Result<u64, Error> {
    conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))
        .map(|n| n as u64)
        .map_err(|e| Error::Storage(e.to_string()))
}

/// Most recent indexed_at across all files.
pub fn max_indexed_at(conn: &Connection) -> Result<i64, Error> {
    conn.query_row(
        "SELECT COALESCE(MAX(indexed_at), 0) FROM files",
        [],
        |row| row.get(0),
    )
    .map_err(|e| Error::Storage(e.to_string()))
}

/// Get symbol counts and top-3 names per file in a single query.
///
/// Single GROUP BY pass with GROUP_CONCAT — fast for up to ~100K symbols.
/// The CASE filter ensures only top-level symbols are concatenated.
pub fn file_symbol_summaries(
    conn: &Connection,
) -> Result<Vec<(PathBuf, usize, Vec<CompactString>)>, Error> {
    let mut stmt = conn
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

/// Load all refs. Interns `Arc<PathBuf>` across rows sharing the same `src_path`.
pub fn all_refs(conn: &Connection) -> Result<Vec<Ref>, Error> {
    let mut stmt = conn
        .prepare_cached("SELECT src_path, src_symbol_id, ident, dst_symbol_id, weight FROM refs")
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

/// Load all symbols. Interns `Arc<PathBuf>` across rows sharing the same path.
pub fn all_symbols(conn: &Connection) -> Result<Vec<Symbol>, Error> {
    let mut stmt = conn
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
pub fn all_file_paths(conn: &Connection) -> Result<Vec<PathBuf>, Error> {
    let mut stmt = conn
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

/// Get file paths filtered by language.
pub fn file_paths_by_lang(conn: &Connection, lang: &str) -> Result<Vec<PathBuf>, Error> {
    let mut stmt = conn
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

/// Look up symbols by a set of IDs — only fetches the rows needed.
/// Returns a Vec of (id, path, line_start) for resolving ref locations.
pub fn symbol_locations_by_ids(
    conn: &Connection,
    ids: &[u64],
) -> Result<Vec<(u64, String, u32)>, Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // Build IN clause with placeholders
    let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT id, path, line_start FROM symbols WHERE id IN ({})",
        placeholders
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| Error::Storage(e.to_string()))?;

    let params: Vec<i64> = ids.iter().map(|id| *id as i64).collect();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            let id: i64 = row.get(0)?;
            let path: String = row.get(1)?;
            let line: u32 = row.get(2)?;
            Ok((id as u64, path, line))
        })
        .map_err(|e| Error::Storage(e.to_string()))?;

    let mut results = Vec::with_capacity(ids.len());
    for r in rows {
        results.push(r.map_err(|e| Error::Storage(e.to_string()))?);
    }
    Ok(results)
}
