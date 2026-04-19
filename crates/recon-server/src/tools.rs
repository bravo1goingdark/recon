//! Tool parameter types for MCP tool calls.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Parameters for `code_outline`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct OutlineParams {
    /// File path relative to repo root.
    pub path: String,
}

/// Parameters for `code_skeleton`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SkeletonParams {
    /// File path relative to repo root.
    pub path: String,
    /// Nesting depth (default 2).
    #[serde(default = "default_depth")]
    pub depth: u32,
}

fn default_depth() -> u32 {
    2
}

/// Parameters for `code_read_symbol`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ReadSymbolParams {
    /// File path relative to repo root.
    pub path: String,
    /// Symbol name or line number.
    pub symbol_or_line: String,
}

/// Parameters for `code_find_symbol`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct FindSymbolParams {
    /// Symbol name to search for (fuzzy matching).
    pub name: String,
    /// Optional symbol kind filter (fn, struct, class, etc).
    pub kind: Option<String>,
    /// Optional language filter.
    pub lang: Option<String>,
}

/// Parameters for `code_find_refs`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct FindRefsParams {
    /// Symbol name or qualified name.
    pub symbol: String,
}

/// Parameters for `code_search`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SearchParams {
    /// Search query string.
    pub query: String,
    /// Search mode: "exact" (default), "regex", "hybrid", or "semantic".
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Optional filter DSL (e.g. "*.rs", "type:rust", "status:modified", "!test").
    pub filter: Option<String>,
}

fn default_mode() -> String {
    "exact".into()
}

/// Parameters for `code_list`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ListParams {
    /// Optional glob pattern to filter files.
    pub glob: Option<String>,
    /// Optional language filter.
    pub lang: Option<String>,
    /// Optional filter DSL (e.g. "*.rs", "type:rust", "status:modified", "!test").
    pub filter: Option<String>,
}

/// Parameters for `code_repo_map`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct RepoMapParams {
    /// Files to focus ranking on.
    pub focus_files: Option<Vec<String>>,
    /// Max tokens for the output (default 2000).
    #[serde(default = "default_budget")]
    pub token_budget: usize,
}

fn default_budget() -> usize {
    2000
}

/// Parameters for `code_find_strings`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct FindStringsParams {
    /// Pattern to search for in string literals/comments.
    pub pattern: String,
    /// Kind: "literal", "comment", or "both" (default).
    #[serde(default = "default_string_kind")]
    pub kind: String,
    /// Optional filter DSL (e.g. "*.rs", "type:rust", "status:modified", "!test").
    pub filter: Option<String>,
}

fn default_string_kind() -> String {
    "both".into()
}

/// Parameters for `code_multi_find`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct MultiFindParams {
    /// Multiple patterns to search simultaneously.
    pub patterns: Vec<String>,
    /// Optional filter DSL (e.g. "*.rs", "type:rust", "status:modified", "!test").
    pub filter: Option<String>,
}

/// Parameters for `code_reindex`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ReindexParams {
    /// Force full re-index even if hashes match. Default false.
    #[serde(default)]
    pub force: bool,
}

/// Parameters for `code_stats`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct StatsParams {}
