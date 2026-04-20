//! SQL schema and migrations.

use rusqlite_migration::{Migrations, M};

/// Build the migration set.
pub fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(SCHEMA_V1), M::up(SCHEMA_V2)])
}

const SCHEMA_V1: &str = r#"
CREATE TABLE files (
    path        TEXT PRIMARY KEY,
    lang        TEXT NOT NULL,
    size_bytes  INTEGER NOT NULL,
    content_hash BLOB NOT NULL,
    mtime       INTEGER NOT NULL,
    indexed_at  INTEGER NOT NULL
);

CREATE TABLE symbols (
    id             INTEGER PRIMARY KEY,
    path           TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    kind           TEXT NOT NULL,
    signature      TEXT,
    doc            TEXT,
    parent_id      INTEGER,
    byte_start     INTEGER NOT NULL,
    byte_end       INTEGER NOT NULL,
    line_start     INTEGER NOT NULL,
    line_end       INTEGER NOT NULL,
    body_hash      BLOB NOT NULL
);

CREATE INDEX symbols_name ON symbols(name COLLATE NOCASE);
CREATE INDEX symbols_qname ON symbols(qualified_name COLLATE NOCASE);
CREATE INDEX symbols_path ON symbols(path);
CREATE INDEX symbols_kind ON symbols(kind);
CREATE INDEX symbols_path_parent ON symbols(path, parent_id);

CREATE VIRTUAL TABLE symbols_fts USING fts5(
    name,
    qualified_name,
    signature,
    doc,
    content='symbols',
    content_rowid='id',
    tokenize='trigram'
);

-- Triggers to keep FTS in sync with content table
CREATE TRIGGER symbols_ai AFTER INSERT ON symbols BEGIN
    INSERT INTO symbols_fts(rowid, name, qualified_name, signature, doc)
    VALUES (new.id, new.name, new.qualified_name, new.signature, new.doc);
END;

CREATE TRIGGER symbols_ad AFTER DELETE ON symbols BEGIN
    INSERT INTO symbols_fts(symbols_fts, rowid, name, qualified_name, signature, doc)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.signature, old.doc);
END;

CREATE TRIGGER symbols_au AFTER UPDATE ON symbols BEGIN
    INSERT INTO symbols_fts(symbols_fts, rowid, name, qualified_name, signature, doc)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.signature, old.doc);
    INSERT INTO symbols_fts(rowid, name, qualified_name, signature, doc)
    VALUES (new.id, new.name, new.qualified_name, new.signature, new.doc);
END;

CREATE TABLE refs (
    src_path       TEXT NOT NULL,
    src_symbol_id  INTEGER NOT NULL,
    ident          TEXT NOT NULL,
    dst_symbol_id  INTEGER,
    weight         REAL NOT NULL DEFAULT 1.0
);

CREATE INDEX refs_ident ON refs(ident);
CREATE INDEX refs_src ON refs(src_symbol_id);
CREATE INDEX refs_dst ON refs(dst_symbol_id);
CREATE INDEX refs_src_ident ON refs(src_symbol_id, ident);

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);

INSERT INTO meta(key, value) VALUES ('schema_version', '1');
"#;

/// V2: Add missing indexes for common query patterns.
/// Note: refs.src_symbol_id uses parser-local IDs, not DB-generated symbol IDs,
/// so we cannot add a FK constraint. Cascade deletion is handled by
/// DELETE FROM symbols WHERE path = ? which cascades from the files FK,
/// plus explicit DELETE FROM refs WHERE src_symbol_id IN (...) in legacy paths.
const SCHEMA_V2: &str = r#"
-- Additional indexes for common query patterns
CREATE INDEX IF NOT EXISTS files_lang ON files(lang);
CREATE INDEX IF NOT EXISTS symbols_parent ON symbols(parent_id) WHERE parent_id IS NOT NULL;

UPDATE meta SET value = '2' WHERE key = 'schema_version';
"#;
