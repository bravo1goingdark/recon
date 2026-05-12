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
    0
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

/// Parameters for `code_path`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct PathParams {
    /// Source symbol name (bare or fully qualified).
    pub src: String,
    /// Destination symbol name (bare or fully qualified).
    pub dst: String,
    /// Maximum hops to explore. Default 8, hard cap 16.
    #[serde(default = "default_max_hops")]
    pub max_hops: u32,
}

fn default_max_hops() -> u32 {
    recon_search::graph::DEFAULT_MAX_HOPS
}

/// Parameters for `code_callers` / `code_callees`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct CallersParams {
    /// Symbol name (bare or fully qualified).
    pub symbol: String,
    /// Depth to explore (default 1, hard cap 6).
    #[serde(default = "default_caller_depth")]
    pub depth: u32,
}

fn default_caller_depth() -> u32 {
    1
}

/// Parameters for `code_context`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ContextParams {
    /// Symbol name or query — bare names, qualified names, or fuzzy queries.
    pub symbol: String,
    /// Token budget for the bundled response. Default 2000.
    #[serde(default = "default_context_budget")]
    pub token_budget: usize,
}

fn default_context_budget() -> usize {
    2000
}

/// Parameters for `code_impact`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ImpactParams {
    /// Symbol name (bare or fully qualified).
    pub symbol: String,
    /// Maximum tier depth (default 4, max 6).
    #[serde(default = "default_impact_depth")]
    pub depth: u32,
}

fn default_impact_depth() -> u32 {
    4
}

/// Parameters for `code_subsystems`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SubsystemsParams {
    /// Maximum number of subsystems to return (default 50).
    #[serde(default = "default_subsystem_limit")]
    pub limit: usize,
}

fn default_subsystem_limit() -> usize {
    50
}

/// Parameters for `code_subsystem`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SubsystemParams {
    /// Subsystem id (from `code_subsystems`).
    pub id: u32,
    /// Token budget for the subsystem skeleton. Default 1500.
    #[serde(default = "default_subsystem_budget")]
    pub token_budget: usize,
}

fn default_subsystem_budget() -> usize {
    1500
}

/// Parameters for `code_savings`. Empty struct — included for shape
/// uniformity with the other tool handlers.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SavingsParams {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outline_params_deserialize() {
        let p: OutlineParams = serde_json::from_str(r#"{"path":"src/lib.rs"}"#).unwrap();
        assert_eq!(p.path, "src/lib.rs");
    }

    #[test]
    fn skeleton_params_default_depth() {
        let p: SkeletonParams = serde_json::from_str(r#"{"path":"src/lib.rs"}"#).unwrap();
        assert_eq!(p.depth, 2);
    }

    #[test]
    fn skeleton_params_custom_depth() {
        let p: SkeletonParams = serde_json::from_str(r#"{"path":"src/lib.rs","depth":5}"#).unwrap();
        assert_eq!(p.depth, 5);
    }

    #[test]
    fn search_params_default_mode() {
        let p: SearchParams = serde_json::from_str(r#"{"query":"foo"}"#).unwrap();
        assert_eq!(p.mode, "exact");
    }

    #[test]
    fn search_params_regex_mode() {
        let p: SearchParams = serde_json::from_str(r#"{"query":"foo.*","mode":"regex"}"#).unwrap();
        assert_eq!(p.mode, "regex");
    }

    #[test]
    fn repo_map_params_default_budget() {
        let p: RepoMapParams = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(p.token_budget, 0);
    }

    #[test]
    fn repo_map_params_custom_budget() {
        let p: RepoMapParams = serde_json::from_str(r#"{"token_budget":500}"#).unwrap();
        assert_eq!(p.token_budget, 500);
    }

    #[test]
    fn find_strings_params_default_kind() {
        let p: FindStringsParams = serde_json::from_str(r#"{"pattern":"TODO"}"#).unwrap();
        assert_eq!(p.kind, "both");
    }

    #[test]
    fn reindex_params_default_force() {
        let p: ReindexParams = serde_json::from_str(r#"{}"#).unwrap();
        assert!(!p.force);
    }

    #[test]
    fn reindex_params_force_true() {
        let p: ReindexParams = serde_json::from_str(r#"{"force":true}"#).unwrap();
        assert!(p.force);
    }

    #[test]
    fn multi_find_params_with_patterns() {
        let p: MultiFindParams = serde_json::from_str(r#"{"patterns":["foo","bar"]}"#).unwrap();
        assert_eq!(p.patterns, vec!["foo", "bar"]);
    }

    #[test]
    fn stats_params_empty() {
        let p: StatsParams = serde_json::from_str(r#"{}"#).unwrap();
        let _ = p;
    }
}
