//! Tool parameter types for MCP tool calls.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct OutlineParams {
    /// File path relative to repo root.
    pub path: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SkeletonParams {
    /// File path relative to repo root.
    pub path: String,
    /// Nesting depth (default 2).
    #[serde(default = "default_depth")]
    pub depth: u32,
}

fn default_depth() -> u32 { 2 }

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ReadSymbolParams {
    /// File path relative to repo root.
    pub path: String,
    /// Symbol name or line number.
    pub symbol_or_line: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct FindSymbolParams {
    /// Symbol name to search for (fuzzy matching).
    pub name: String,
    /// Optional symbol kind filter (fn, struct, class, etc).
    pub kind: Option<String>,
    /// Optional language filter.
    pub lang: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct FindRefsParams {
    /// Symbol name or qualified name.
    pub symbol: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SearchParams {
    /// Search query string.
    pub query: String,
    /// Search mode: "exact", "regex", or "hybrid" (default).
    #[serde(default = "default_mode")]
    pub mode: String,
}

fn default_mode() -> String { "exact".into() }

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ListParams {
    /// Optional glob pattern to filter files.
    pub glob: Option<String>,
    /// Optional language filter.
    pub lang: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct RepoMapParams {
    /// Files to focus ranking on.
    pub focus_files: Option<Vec<String>>,
    /// Max tokens for the output (default 2000).
    #[serde(default = "default_budget")]
    pub token_budget: usize,
}

fn default_budget() -> usize { 2000 }

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct FindStringsParams {
    /// Pattern to search for in string literals/comments.
    pub pattern: String,
    /// Kind: "literal", "comment", or "both" (default).
    #[serde(default = "default_string_kind")]
    pub kind: String,
}

fn default_string_kind() -> String { "both".into() }

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct MultiFindParams {
    /// Multiple patterns to search simultaneously.
    pub patterns: Vec<String>,
}
