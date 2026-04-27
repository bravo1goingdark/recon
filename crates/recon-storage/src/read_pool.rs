//! Lock-free pool of read-only SQLite connections for concurrent queries.
//!
//! Uses `crossbeam_queue::ArrayQueue` (same pattern as `recon-parser`'s `ParserPool`)
//! so that multiple tool handlers can read the symbol database simultaneously
//! without contending on a single `Mutex<Store>`. WAL mode (already enabled in
//! `Store::init`) ensures read-only connections see a consistent snapshot even
//! while the write connection is committing.

use crate::read_fns;
use compact_str::CompactString;
use crossbeam_queue::ArrayQueue;
use recon_core::error::Error;
use recon_core::symbol::{Ref, Symbol};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// A lock-free pool of read-only SQLite connections.
///
/// `with()` pops a connection from the queue, runs a closure, and pushes it
/// back. If the pool is empty a new overflow connection is created on the fly
/// (same design as `ParserPool`). This means callers never block — the pool
/// only avoids redundant connection opens under normal concurrency.
pub struct ReadPool {
    db_path: PathBuf,
    pool: ArrayQueue<Connection>,
}

impl ReadPool {
    /// Create a pool with `capacity` pre-opened read-only connections.
    pub fn new(db_path: &Path, capacity: usize) -> Result<Self, Error> {
        let pool = ArrayQueue::new(capacity);
        for _ in 0..capacity {
            let conn = Self::open_read_conn(db_path)?;
            // Best-effort push; if pool is somehow full, drop the extra
            let _ = pool.push(conn);
        }
        Ok(Self {
            db_path: db_path.to_path_buf(),
            pool,
        })
    }

    /// Borrow a connection, run a closure, return the connection. Lock-free.
    ///
    /// If the pool is empty and a new connection cannot be opened (DB deleted,
    /// FD exhaustion), returns an error instead of panicking.
    pub fn with<R>(&self, f: impl FnOnce(&Connection) -> Result<R, Error>) -> Result<R, Error> {
        let conn = match self.pool.pop() {
            Some(c) => c,
            None => Self::open_read_conn(&self.db_path)?,
        };
        let result = f(&conn);
        // Best-effort return; if pool is full, connection is dropped (acceptable)
        let _ = self.pool.push(conn);
        result
    }

    /// Open a single read-only connection with optimized pragmas.
    fn open_read_conn(db_path: &Path) -> Result<Connection, Error> {
        let conn = Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| Error::Storage(format!("read pool open: {e}")))?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA cache_size=-32000;
             PRAGMA mmap_size=268435456;
             PRAGMA temp_store=MEMORY;
             PRAGMA query_only=ON;",
        )
        .map_err(|e| Error::Storage(format!("read pool pragmas: {e}")))?;

        Ok(conn)
    }

    // ── Convenience wrappers that delegate to read_fns ──

    /// List all symbols for a path.
    pub fn symbols_for_path(&self, path: &Path) -> Result<Vec<Symbol>, Error> {
        self.with(|conn| read_fns::symbols_for_path(conn, path))
    }

    /// Find symbols by exact name (case-insensitive).
    pub fn find_symbols_exact(&self, name: &str, limit: usize) -> Result<Vec<Symbol>, Error> {
        self.with(|conn| read_fns::find_symbols_exact(conn, name, limit))
    }

    /// Fuzzy search symbols via FTS5 trigram.
    pub fn search_symbols_fuzzy(&self, query: &str, limit: usize) -> Result<Vec<Symbol>, Error> {
        self.with(|conn| read_fns::search_symbols_fuzzy(conn, query, limit))
    }

    /// Look up a symbol by qualified name.
    pub fn get_symbol_by_qname(&self, qname: &str) -> Result<Option<Symbol>, Error> {
        self.with(|conn| read_fns::get_symbol_by_qname(conn, qname))
    }

    /// Look up a symbol by its numeric ID.
    pub fn symbol_by_id(&self, id: u64) -> Result<Option<Symbol>, Error> {
        self.with(|conn| read_fns::symbol_by_id(conn, id))
    }

    /// Get the docstring for a symbol from the separate symbol_docs table.
    pub fn symbol_doc_by_id(&self, id: u64) -> Result<Option<String>, Error> {
        self.with(|conn| read_fns::symbol_doc_by_id(conn, id))
    }

    /// Find all refs for a given identifier.
    pub fn refs_for_ident(&self, ident: &str) -> Result<Vec<Ref>, Error> {
        self.with(|conn| read_fns::refs_for_ident(conn, ident))
    }

    /// Get file content hash.
    pub fn get_file_hash(&self, path: &Path) -> Result<Option<[u8; 32]>, Error> {
        self.with(|conn| read_fns::get_file_hash(conn, path))
    }

    /// Get a meta key.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>, Error> {
        self.with(|conn| read_fns::get_meta(conn, key))
    }

    /// Count all symbols.
    pub fn symbol_count(&self) -> Result<u64, Error> {
        self.with(read_fns::symbol_count)
    }

    /// Count all indexed files.
    pub fn file_count(&self) -> Result<u64, Error> {
        self.with(read_fns::file_count)
    }

    /// Most recent indexed_at across all files.
    pub fn max_indexed_at(&self) -> Result<i64, Error> {
        self.with(read_fns::max_indexed_at)
    }

    /// Get symbol counts and top-3 names per file.
    pub fn file_symbol_summaries(
        &self,
    ) -> Result<Vec<(PathBuf, usize, Vec<CompactString>)>, Error> {
        self.with(read_fns::file_symbol_summaries)
    }

    /// Load all refs.
    pub fn all_refs(&self) -> Result<Vec<Ref>, Error> {
        self.with(read_fns::all_refs)
    }

    /// Load all symbols.
    pub fn all_symbols(&self) -> Result<Vec<Symbol>, Error> {
        self.with(read_fns::all_symbols)
    }

    /// List all indexed file paths.
    pub fn all_file_paths(&self) -> Result<Vec<PathBuf>, Error> {
        self.with(read_fns::all_file_paths)
    }

    /// Snapshot all paths, symbols, and refs from a single point-in-time.
    ///
    /// Wraps three reads in one transaction on a single connection so the
    /// returned tuples reflect the same SQLite state — without this, a
    /// concurrent writer can interleave between the three queries and leave
    /// the caches mutually inconsistent (e.g. symbols referencing a path no
    /// longer in the path list). WAL mode gives this snapshot for free.
    #[allow(clippy::type_complexity)]
    pub fn snapshot_all_for_caches(&self) -> Result<(Vec<PathBuf>, Vec<Symbol>, Vec<Ref>), Error> {
        self.with(|conn| {
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| Error::Storage(e.to_string()))?;
            let paths = read_fns::all_file_paths(&tx)?;
            let symbols = read_fns::all_symbols(&tx)?;
            let refs = read_fns::all_refs(&tx)?;
            tx.commit().map_err(|e| Error::Storage(e.to_string()))?;
            Ok((paths, symbols, refs))
        })
    }

    /// Get file paths filtered by language.
    pub fn file_paths_by_lang(&self, lang: &str) -> Result<Vec<PathBuf>, Error> {
        self.with(|conn| read_fns::file_paths_by_lang(conn, lang))
    }

    /// Look up (id, path, line_start) for a set of symbol IDs.
    /// Much cheaper than loading all symbols when you only need location data.
    pub fn symbol_locations_by_ids(&self, ids: &[u64]) -> Result<Vec<(u64, String, u32)>, Error> {
        self.with(|conn| read_fns::symbol_locations_by_ids(conn, ids))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use compact_str::CompactString;
    use recon_core::lang::Language;
    use recon_core::symbol::{FileMeta, SymbolKind};
    use std::sync::Arc;

    fn setup_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let store = Store::open(&db_path).unwrap();

        // Insert test data
        let meta = FileMeta {
            path: PathBuf::from("src/lib.rs"),
            lang: Language::Rust,
            size_bytes: 1024,
            content_hash: [0u8; 32],
            mtime: 1000,
            indexed_at: 1001,
        };
        store.upsert_file(&meta).unwrap();

        let sym = Symbol {
            id: 0,
            path: Arc::new(PathBuf::from("src/lib.rs")),
            name: CompactString::new("validate_email"),
            qualified_name: CompactString::new("mod::validate_email"),
            kind: SymbolKind::Function,
            signature: Some("fn validate_email()".into()),
            doc: None,
            parent_id: None,
            byte_range: 0..100,
            line_range: 1..=10,
            body_hash: [0u8; 32],
            lang: Language::Rust,
        };
        store.insert_symbol(&sym).unwrap();

        (dir, db_path)
    }

    #[test]
    fn read_pool_basic() {
        let (_dir, db_path) = setup_db();
        let pool = ReadPool::new(&db_path, 2).unwrap();

        let count = pool.symbol_count().unwrap();
        assert_eq!(count, 1);

        let syms = pool.symbols_for_path(Path::new("src/lib.rs")).unwrap();
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name.as_str(), "validate_email");
    }

    #[test]
    fn concurrent_readers() {
        let (_dir, db_path) = setup_db();
        let pool = Arc::new(ReadPool::new(&db_path, 4).unwrap());

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let pool = Arc::clone(&pool);
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        let count = pool.symbol_count().unwrap();
                        assert_eq!(count, 1);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn pool_overflow_creates_connections() {
        let (_dir, db_path) = setup_db();
        // Pool of 1, but 4 concurrent readers
        let pool = Arc::new(ReadPool::new(&db_path, 1).unwrap());

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let pool = Arc::clone(&pool);
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let count = pool.symbol_count().unwrap();
                        assert_eq!(count, 1);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn snapshot_all_for_caches_returns_consistent_view() {
        let (_dir, db_path) = setup_db();
        let pool = ReadPool::new(&db_path, 2).unwrap();

        let (paths, symbols, refs) = pool.snapshot_all_for_caches().unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name.as_str(), "validate_email");
        assert!(refs.is_empty());
    }

    #[test]
    fn snapshot_all_for_caches_on_empty_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("empty.db");
        let _store = Store::open(&db_path).unwrap();
        let pool = ReadPool::new(&db_path, 2).unwrap();

        let (paths, symbols, refs) = pool.snapshot_all_for_caches().unwrap();
        assert!(paths.is_empty());
        assert!(symbols.is_empty());
        assert!(refs.is_empty());
    }

    #[test]
    fn reader_writer_isolation() {
        let (_dir, db_path) = setup_db();
        let pool = Arc::new(ReadPool::new(&db_path, 2).unwrap());
        let store = Store::open(&db_path).unwrap();

        // Writer adds a new symbol
        let sym2 = Symbol {
            id: 0,
            path: Arc::new(PathBuf::from("src/lib.rs")),
            name: CompactString::new("send_email"),
            qualified_name: CompactString::new("mod::send_email"),
            kind: SymbolKind::Function,
            signature: Some("fn send_email()".into()),
            doc: None,
            parent_id: None,
            byte_range: 100..200,
            line_range: 11..=20,
            body_hash: [1u8; 32],
            lang: Language::Rust,
        };
        store.insert_symbol(&sym2).unwrap();

        // Reader should see the new data (WAL readers see committed writes)
        let count = pool.symbol_count().unwrap();
        assert_eq!(count, 2);
    }
}
