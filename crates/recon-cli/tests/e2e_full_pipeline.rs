//! E2E integration tests — full indexing pipeline + tool validation.
//!
//! These tests build a realistic multi-language project in a temp directory,
//! run the full indexer, and validate tool outputs against known ground truth.

use std::fs;
use std::path::Path;

use recon_search::tantivy_backend::TantivyBackend;
use recon_server::server::ReconServer;
use recon_storage::store::Store;

/// Parse the hits array out of a row-oriented tool response. Accepts both
/// the v0.5+ canonical `Hits` envelope (`{shape:"Hits", kind, count, hits}`)
/// and a bare JSON array — useful while not every test fixture has been
/// updated to the new wire shape yet.
fn parse_hits(result: &str) -> Vec<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(result)
        .unwrap_or_else(|e| panic!("response is not JSON: {e}\nbody: {result}"));
    if let Some(arr) = v.as_array() {
        return arr.clone();
    }
    if v.get("shape").and_then(|s| s.as_str()) == Some("Hits") {
        return v["hits"].as_array().cloned().unwrap_or_default();
    }
    panic!("expected array or Hits envelope, got: {result}");
}

/// Write a file, creating parent directories as needed.
fn fs_write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

/// Build a realistic multi-file Rust project in a temp directory.
fn build_rust_project(root: &Path) {
    fs_write(
        root,
        "Cargo.toml",
        r#"[package]
name = "tiny-lib"
version = "0.1.0"
edition = "2021"
"#,
    );

    fs_write(
        root,
        "src/lib.rs",
        r#"//! A tiny utility library for demonstration.

pub mod config;
pub mod parser;
pub mod utils;

pub use config::Config;
pub use parser::Parser;
pub use utils::Result;
"#,
    );

    fs_write(
        root,
        "src/config.rs",
        r#"use std::collections::HashMap;

/// Application configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Key-value settings.
    pub settings: HashMap<String, String>,
    /// Debug mode flag.
    pub debug: bool,
}

impl Config {
    /// Create a new empty configuration.
    pub fn new() -> Self {
        Self {
            settings: HashMap::new(),
            debug: false,
        }
    }

    /// Set a configuration value.
    pub fn set(&mut self, key: &str, value: &str) {
        self.settings.insert(key.to_string(), value.to_string());
    }

    /// Get a configuration value.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.settings.get(key).map(|s| s.as_str())
    }

    /// Enable debug mode.
    pub fn with_debug(mut self) -> Self {
        self.debug = true;
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}
"#,
    );

    fs_write(
        root,
        "src/parser.rs",
        r#"use crate::config::Config;
use crate::utils::Result;

/// A simple line-based parser.
pub struct Parser {
    config: Config,
    lines: Vec<String>,
}

impl Parser {
    /// Create a new parser with the given configuration.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            lines: Vec::new(),
        }
    }

    /// Parse input text into lines.
    pub fn parse(&mut self, input: &str) -> Result<()> {
        self.lines = input.lines().map(|s| s.to_string()).collect();
        if self.config.debug {
            eprintln!("Parsed {} lines", self.lines.len());
        }
        Ok(())
    }

    /// Get the number of parsed lines.
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Get a specific line by index.
    pub fn line_at(&self, index: usize) -> Option<&str> {
        self.lines.get(index).map(|s| s.as_str())
    }

    /// Find lines containing the given pattern.
    pub fn find_lines(&self, pattern: &str) -> Vec<&str> {
        self.lines
            .iter()
            .filter(|line| line.contains(pattern))
            .map(|s| s.as_str())
            .collect()
    }
}

/// Parse a string and return the line count.
pub fn quick_parse(input: &str) -> usize {
    input.lines().count()
}
"#,
    );

    fs_write(
        root,
        "src/utils.rs",
        r#"use std::fmt;

/// A result type for this library.
pub type Result<T> = std::result::Result<T, Error>;

/// Error type for this library.
#[derive(Debug)]
pub enum Error {
    /// Input was empty.
    EmptyInput,
    /// Parse error at the given line.
    ParseError { line: usize, message: String },
    /// Configuration error.
    ConfigError(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::EmptyInput => write!(f, "input was empty"),
            Error::ParseError { line, message } => {
                write!(f, "parse error at line {line}: {message}")
            }
            Error::ConfigError(msg) => write!(f, "config error: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

/// Trim whitespace from both ends of a string.
pub fn trim(s: &str) -> &str {
    s.trim()
}

/// Check if a string is a valid identifier.
pub fn is_valid_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().map_or(false, |c| c.is_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Split a string by a delimiter, trimming each part.
pub fn split_trim(s: &str, delimiter: char) -> Vec<&str> {
    s.split(delimiter).map(trim).collect()
}
"#,
    );
}

/// Build a multi-language project with Rust, Python, and Go files.
fn build_multi_lang_project(root: &Path) {
    fs_write(
        root,
        "src/main.rs",
        r#"fn main() {
    println!("Hello from recon!");
    let result = add(2, 3);
    println!("2 + 3 = {result}");
}

fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn multiply(a: i32, b: i32) -> i32 {
    a * b
}
"#,
    );

    fs_write(
        root,
        "scripts/helper.py",
        r#"#!/usr/bin/env python3
\"\"\"Helper utilities for the project.\"\"\"

import os
import sys


def find_files(directory: str, extension: str) -> list[str]:
    \"\"\"Find all files with the given extension in a directory.\"\"\"
    result = []
    for root, _dirs, files in os.walk(directory):
        for file in files:
            if file.endswith(extension):
                result.append(os.path.join(root, file))
    return result


def format_output(data: list) -> str:
    \"\"\"Format a list of items as a readable string.\"\"\"
    return \"\\n\".join(f\"  - {item}\" for item in data)


class Config:
    \"\"\"Simple configuration holder.\"\"\"

    def __init__(self, debug: bool = False):
        self.debug = debug
        self.verbose = False

    def enable_verbose(self):
        self.verbose = True
"#,
    );

    fs_write(
        root,
        "cmd/server/main.go",
        r#"package main

import (
	"fmt"
	"net/http"
)

// Handler processes HTTP requests.
type Handler struct {
	prefix string
}

// NewHandler creates a new Handler with the given prefix.
func NewHandler(prefix string) *Handler {
	return &Handler{prefix: prefix}
}

// ServeHTTP implements the http.Handler interface.
func (h *Handler) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	fmt.Fprintf(w, "%s: %s", h.prefix, r.URL.Path)
}

func main() {
	handler := NewHandler("recon")
	http.Handle("/api", handler)
	http.ListenAndServe(":8080", nil)
}
"#,
    );
}

/// Create a server with an on-disk store at the given root.
async fn make_server(root: &Path) -> ReconServer {
    let recon_dir = root.join(".recon");
    fs::create_dir_all(&recon_dir).unwrap();

    let db_path = recon_dir.join("recon.db");
    let store = Store::open(&db_path).unwrap();

    let tantivy_dir = recon_dir.join("tantivy");
    fs::create_dir_all(&tantivy_dir).unwrap();
    let tantivy = TantivyBackend::open(&tantivy_dir).unwrap();

    ReconServer::new(root.to_path_buf(), store, tantivy).unwrap()
}

#[tokio::test]
async fn e2e_rust_project_index_and_query() {
    let tmp = tempfile::tempdir().unwrap();
    build_rust_project(tmp.path());

    let server = make_server(tmp.path()).await;
    server.index_repo().await.unwrap();

    // 1. code_stats: verify indexing counts
    let stats = server.query_tool("code_stats", "{}").await;
    let stats_json: serde_json::Value = serde_json::from_str(&stats).unwrap();
    let files = stats_json["files_indexed"].as_u64().unwrap();
    let symbols = stats_json["total_symbols"].as_u64().unwrap();
    assert!(
        files >= 3,
        "should index at least 3 source files, got {files}"
    );
    assert!(
        symbols >= 10,
        "should index at least 10 symbols, got {symbols}"
    );

    // 2. code_find_symbol: find Config struct
    let result = server
        .query_tool(
            "code_find_symbol",
            r#"{"name": "Config", "kind": null, "lang": null}"#,
        )
        .await;
    let entries = parse_hits(&result);
    assert!(!entries.is_empty(), "should find Config symbol: {result}");

    // 3. code_outline: check src/config.rs structure
    let result = server
        .query_tool("code_outline", r#"{"path": "src/config.rs"}"#)
        .await;
    let json: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(json["shape"].as_str(), Some("Outline"));
    let entries = json["entries"].as_array().unwrap();
    let names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(names.contains(&"Config"), "should have Config: {names:?}");
    // impl methods appear as separate top-level entries (e.g., "impl Config")
    assert!(
        names
            .iter()
            .any(|n| n.contains("impl") || n.contains("Config")),
        "should have impl blocks or Config: {names:?}"
    );

    // 4. code_read_symbol: read Config::new
    let result = server
        .query_tool(
            "code_read_symbol",
            r#"{"path": "src/config.rs", "symbol_or_line": "new"}"#,
        )
        .await;
    let json: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(json["shape"].as_str(), Some("SymbolCard"));
    assert!(json["body"].as_str().unwrap().contains("HashMap::new()"));

    // 5. code_skeleton: check elided bodies
    let result = server
        .query_tool("code_skeleton", r#"{"path": "src/parser.rs", "depth": 1}"#)
        .await;
    let json: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(json["shape"].as_str(), Some("Skeleton"));
    assert!(json["content"].as_str().unwrap().contains("{ ... }"));

    // 6. code_search: find "HashMap" usage
    let result = server
        .query_tool(
            "code_search",
            r#"{"query": "HashMap", "mode": "exact", "filter": null}"#,
        )
        .await;
    let entries = parse_hits(&result);
    assert!(
        !entries.is_empty(),
        "should find HashMap references: {result}"
    );

    // 7. code_list: list Rust files
    let result = server
        .query_tool(
            "code_list",
            r#"{"lang": "rust", "filter": null, "glob": null}"#,
        )
        .await;
    let entries = parse_hits(&result);
    assert!(
        entries.len() >= 3,
        "should list at least 3 Rust files, got {}",
        entries.len()
    );

    // 8. code_repo_map: get ranked overview
    let result = server
        .query_tool(
            "code_repo_map",
            r#"{"focus_files": null, "token_budget": 500}"#,
        )
        .await;
    let json: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(json["shape"].as_str(), Some("Skeleton"));
    assert!(!json["content"].as_str().unwrap().is_empty());

    // 9. code_reindex: force reindex
    let result = server
        .query_tool("code_reindex", r#"{"force": true}"#)
        .await;
    let reindex: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(reindex["status"].as_str(), Some("ok"));
    assert_eq!(reindex["force"].as_bool(), Some(true));

    // 10. code_find_refs: find references to Config
    let result = server
        .query_tool("code_find_refs", r#"{"symbol": "Config"}"#)
        .await;
    let json: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(json["shape"].as_str(), Some("ReferenceDigest"));
    assert_eq!(json["symbol"].as_str(), Some("Config"));
}

#[tokio::test]
async fn e2e_multi_language_project() {
    let tmp = tempfile::tempdir().unwrap();
    build_multi_lang_project(tmp.path());

    let server = make_server(tmp.path()).await;
    server.index_repo().await.unwrap();

    // 1. Verify all three languages are indexed
    let stats = server.query_tool("code_stats", "{}").await;
    let stats_json: serde_json::Value = serde_json::from_str(&stats).unwrap();
    let files = stats_json["files_indexed"].as_u64().unwrap();
    assert!(
        files >= 3,
        "should index at least 3 files across languages, got {files}"
    );

    // 2. Rust symbols
    let result = server
        .query_tool(
            "code_find_symbol",
            r#"{"name": "add", "kind": null, "lang": "rust"}"#,
        )
        .await;
    let entries = parse_hits(&result);
    assert!(
        !entries.is_empty(),
        "should find Rust 'add' function: {result}"
    );

    // 3. Python symbols
    let result = server
        .query_tool(
            "code_find_symbol",
            r#"{"name": "find_files", "kind": null, "lang": "python"}"#,
        )
        .await;
    let entries = parse_hits(&result);
    assert!(
        !entries.is_empty(),
        "should find Python 'find_files' function: {result}"
    );

    // 4. Go symbols
    let result = server
        .query_tool(
            "code_find_symbol",
            r#"{"name": "NewHandler", "kind": null, "lang": "go"}"#,
        )
        .await;
    let entries = parse_hits(&result);
    assert!(
        !entries.is_empty(),
        "should find Go 'NewHandler' function: {result}"
    );

    // 5. code_search with regex across languages
    let result = server
        .query_tool(
            "code_search",
            r#"{"query": "fn\\s+\\w+", "mode": "regex", "filter": null}"#,
        )
        .await;
    let entries = parse_hits(&result);
    assert!(
        !entries.is_empty(),
        "regex search should find Rust functions: {result}"
    );

    // 6. code_multi_find: search multiple patterns
    let result = server
        .query_tool(
            "code_multi_find",
            r#"{"patterns": ["fn add", "fn multiply"], "filter": null}"#,
        )
        .await;
    let results = parse_hits(&result);
    assert!(
        !results.is_empty(),
        "multi_find should return results: {result}"
    );

    // 7. code_list with language filter
    let result = server
        .query_tool(
            "code_list",
            r#"{"lang": "python", "filter": null, "glob": null}"#,
        )
        .await;
    let entries = parse_hits(&result);
    assert!(
        !entries.is_empty(),
        "should list at least 1 Python file: {result}"
    );
}

#[tokio::test]
async fn e2e_filter_dsl() {
    let tmp = tempfile::tempdir().unwrap();
    build_rust_project(tmp.path());

    let server = make_server(tmp.path()).await;
    server.index_repo().await.unwrap();

    // 1. Search with glob filter
    let result = server
        .query_tool(
            "code_search",
            r#"{"query": "pub fn", "mode": "exact", "filter": "*.rs"}"#,
        )
        .await;
    assert!(
        !result.starts_with("Error:"),
        "glob filter should work: {result}"
    );

    // 2. Search with language filter in DSL
    let result = server
        .query_tool(
            "code_search",
            r#"{"query": "pub", "mode": "exact", "filter": "type:rust"}"#,
        )
        .await;
    assert!(
        !result.starts_with("Error:"),
        "language filter should work: {result}"
    );

    // 3. code_list with glob filter
    let result = server
        .query_tool(
            "code_list",
            r#"{"lang": null, "filter": null, "glob": "config"}"#,
        )
        .await;
    let entries = parse_hits(&result);
    assert!(
        !entries.is_empty(),
        "glob filter should match config.rs: {result}"
    );
}

#[tokio::test]
async fn e2e_incremental_add_file() {
    let tmp = tempfile::tempdir().unwrap();
    build_rust_project(tmp.path());

    let server = make_server(tmp.path()).await;
    server.index_repo().await.unwrap();

    // Get initial stats
    let stats_before = server.query_tool("code_stats", "{}").await;
    let json_before: serde_json::Value = serde_json::from_str(&stats_before).unwrap();
    let files_before = json_before["files_indexed"].as_u64().unwrap();

    // Add a new file
    fs_write(
        tmp.path(),
        "src/new_module.rs",
        r#"/// A brand new module.
pub fn brand_new_function() -> i32 {
    42
}

pub struct NewStruct {
    pub value: i32,
}
"#,
    );

    // Run incremental reindex (not force)
    let result = server
        .query_tool("code_reindex", r#"{"force": false}"#)
        .await;
    let reindex: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(reindex["status"].as_str(), Some("ok"));

    // Verify the new file is indexed
    let find_result = server
        .query_tool(
            "code_find_symbol",
            r#"{"name": "brand_new_function", "kind": null, "lang": null}"#,
        )
        .await;
    let entries = parse_hits(&find_result);
    assert!(
        !entries.is_empty(),
        "should find brand_new_function after incremental reindex: {find_result}"
    );

    // File count should have increased
    let stats_after = server.query_tool("code_stats", "{}").await;
    let json_after: serde_json::Value = serde_json::from_str(&stats_after).unwrap();
    let files_after = json_after["files_indexed"].as_u64().unwrap();
    assert!(
        files_after > files_before,
        "file count should increase after adding a file: before={files_before}, after={files_after}"
    );
}

#[tokio::test]
async fn e2e_path_traversal_denied() {
    let tmp = tempfile::tempdir().unwrap();
    build_rust_project(tmp.path());

    let server = make_server(tmp.path()).await;
    server.index_repo().await.unwrap();

    // Attempt to read a file outside the repo
    let result = server
        .query_tool("code_outline", r#"{"path": "../../../etc/passwd"}"#)
        .await;
    let err: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        err["shape"], "Error",
        "should be structured error: {result}"
    );
    // The path doesn't exist → canonicalize fails → NotFound (-32002).
    // A path that DID exist and escaped the repo would return PathTraversal
    // (-32007). Either is a valid denial — assert the numeric code is one of
    // the two security-relevant codes, not a success.
    let code = err["code"].as_i64().unwrap();
    assert!(
        code == -32002 || code == -32007,
        "path traversal attempt should yield NotFound or PathTraversal, got code={code}: {result}"
    );
}

#[tokio::test]
async fn e2e_sensitive_file_redaction() {
    let tmp = tempfile::tempdir().unwrap();
    build_rust_project(tmp.path());

    // Add a sensitive file
    fs_write(
        tmp.path(),
        ".env",
        "DATABASE_URL=postgres://user:secret@localhost/db\nAPI_KEY=sk-1234567890abcdef\n",
    );

    let server = make_server(tmp.path()).await;
    server.index_repo().await.unwrap();

    // Sensitive files should not appear in listings
    let result = server
        .query_tool(
            "code_list",
            r#"{"lang": null, "filter": null, "glob": ".env"}"#,
        )
        .await;
    let entries = parse_hits(&result);
    assert!(
        entries.is_empty(),
        ".env file should not be indexed: {result}"
    );
}

/// Two identical hybrid-search calls must return byte-identical responses.
///
/// Hybrid mode fuses Tantivy BM25 and text-grep hits via reciprocal rank
/// fusion. The fusion table was historically an `AHashMap`, so ties on the
/// fused score were broken by hash iteration order — non-deterministic
/// across runs, which busts the LLM client's prompt cache for repeated
/// queries. Switching to `BTreeMap` gives deterministic key order and
/// therefore byte-stable JSON.
#[tokio::test]
async fn e2e_hybrid_search_deterministic() {
    let tmp = tempfile::tempdir().unwrap();
    build_rust_project(tmp.path());

    let server = make_server(tmp.path()).await;
    server.index_repo().await.unwrap();

    let args = r#"{"query": "Config", "mode": "hybrid", "filter": null}"#;
    let first = server.query_tool("code_search", args).await;
    let second = server.query_tool("code_search", args).await;

    assert_eq!(
        first, second,
        "hybrid search must be byte-stable across calls\nfirst:  {first}\nsecond: {second}"
    );
}
