//! Search filter DSL powered by `fff-query-parser`.
//!
//! Parses constraint strings like `"*.rs"`, `"status:modified"`, `"!test"`
//! and applies them to narrow file scope before text search.

use globset::{Glob, GlobSet, GlobSetBuilder};
use recon_core::error::Error;
use std::path::{Path, PathBuf};

/// Parsed search constraints extracted from a filter string.
#[derive(Debug, Clone, Default)]
pub struct ParsedFilter {
    /// Extension whitelist (e.g. `["rs", "py"]`).
    pub extensions: Vec<String>,
    /// File type names (e.g. `["rust"]`).
    pub file_types: Vec<String>,
    /// Glob patterns.
    pub globs: Vec<String>,
    /// Path segment constraints (e.g. `"src"` from `/src/`).
    pub path_segments: Vec<String>,
    /// Negated text patterns.
    pub exclude_texts: Vec<String>,
    /// Whether to filter to git-modified files only.
    pub git_modified_only: bool,
}

/// Parse a filter string into structured constraints using fff-query-parser.
pub fn parse_filter(filter: &str) -> Result<ParsedFilter, Error> {
    let parser = fff_query_parser::QueryParser::default();
    let result = parser.parse(filter);

    let mut parsed = ParsedFilter::default();

    for constraint in &result.constraints {
        match constraint {
            fff_query_parser::Constraint::Extension(ext) => {
                parsed.extensions.push(ext.to_string());
            }
            fff_query_parser::Constraint::Glob(g) => {
                parsed.globs.push(g.to_string());
            }
            fff_query_parser::Constraint::FileType(ft) => {
                parsed.file_types.push(ft.to_string());
            }
            fff_query_parser::Constraint::PathSegment(seg) => {
                parsed.path_segments.push(seg.to_string());
            }
            fff_query_parser::Constraint::GitStatus(
                fff_query_parser::GitStatusFilter::Modified,
            ) => {
                parsed.git_modified_only = true;
            }
            fff_query_parser::Constraint::Not(inner) => {
                if let fff_query_parser::Constraint::Text(t) = inner.as_ref() {
                    parsed.exclude_texts.push(t.to_string());
                }
            }
            _ => {
                // Text, Parts, Exclude, other git statuses — skip for now
            }
        }
    }

    Ok(parsed)
}

/// Apply parsed filter constraints to a list of file paths.
///
/// Returns only paths that match all constraints.
pub fn apply_filter(paths: &[PathBuf], filter: &ParsedFilter) -> Vec<PathBuf> {
    // Compile glob patterns once for the whole batch to avoid per-path re-compilation.
    let glob_set = build_glob_set(&filter.globs);
    paths
        .iter()
        .filter(|p| matches_filter(p, filter, glob_set.as_ref()))
        .cloned()
        .collect()
}

/// Compile a list of glob patterns into a `GlobSet`.
///
/// Patterns that fail to parse are silently skipped (already validated by
/// `fff-query-parser`). Returns `None` when the pattern list is empty.
fn build_glob_set(patterns: &[String]) -> Option<GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        if let Ok(g) = Glob::new(p) {
            builder.add(g);
        }
    }
    builder.build().ok()
}

fn matches_filter(path: &Path, filter: &ParsedFilter, glob_set: Option<&GlobSet>) -> bool {
    let path_str = path.to_string_lossy();

    // Extension filter
    if !filter.extensions.is_empty() {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !filter.extensions.iter().any(|e| e == ext) {
            return false;
        }
    }

    // File type filter (map common type names to extensions)
    if !filter.file_types.is_empty() {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let matches_type = filter.file_types.iter().any(|ft| match ft.as_str() {
            "rust" => ext == "rs",
            "python" => ext == "py",
            "typescript" => ext == "ts" || ext == "tsx",
            "javascript" => ext == "js" || ext == "jsx",
            "go" => ext == "go",
            "java" => ext == "java",
            "c" => ext == "c" || ext == "h",
            "cpp" => ext == "cpp" || ext == "hpp" || ext == "cc" || ext == "cxx",
            _ => ext == ft.as_str(),
        });
        if !matches_type {
            return false;
        }
    }

    // Path segment filter
    if !filter.path_segments.is_empty()
        && !filter
            .path_segments
            .iter()
            .all(|seg| path_str.contains(seg))
    {
        return false;
    }

    // Glob filter — matched against the full path string.
    if let Some(gs) = glob_set {
        if !gs.is_match(path) {
            return false;
        }
    }

    // Exclude text filter
    for excl in &filter.exclude_texts {
        if path_str.contains(excl.as_str()) {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extension_filter() {
        let f = parse_filter("*.rs").unwrap();
        assert_eq!(f.extensions, vec!["rs"]);
    }

    #[test]
    fn parse_file_type_filter() {
        let f = parse_filter("type:rust foo").unwrap();
        assert_eq!(f.file_types, vec!["rust"]);
    }

    #[test]
    fn parse_git_status() {
        let f = parse_filter("status:modified foo").unwrap();
        assert!(f.git_modified_only);
    }

    #[test]
    fn parse_negation() {
        let f = parse_filter("!test foo").unwrap();
        assert_eq!(f.exclude_texts, vec!["test"]);
    }

    #[test]
    fn parse_path_segment() {
        let f = parse_filter("/src/ foo").unwrap();
        assert_eq!(f.path_segments, vec!["src"]);
    }

    #[test]
    fn apply_extension_filter() {
        let paths = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("src/lib.py"),
            PathBuf::from("src/util.rs"),
        ];
        let f = ParsedFilter {
            extensions: vec!["rs".into()],
            ..Default::default()
        };
        let result = apply_filter(&paths, &f);
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|p| p.extension().unwrap() == "rs"));
    }

    #[test]
    fn apply_exclude_filter() {
        let paths = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("test/test_main.rs"),
            PathBuf::from("src/util.rs"),
        ];
        let f = ParsedFilter {
            exclude_texts: vec!["test".into()],
            ..Default::default()
        };
        let result = apply_filter(&paths, &f);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn apply_type_filter() {
        let paths = vec![
            PathBuf::from("main.rs"),
            PathBuf::from("main.py"),
            PathBuf::from("main.go"),
        ];
        let f = ParsedFilter {
            file_types: vec!["rust".into()],
            ..Default::default()
        };
        let result = apply_filter(&paths, &f);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], PathBuf::from("main.rs"));
    }

    #[test]
    fn empty_filter_passes_all() {
        let paths = vec![PathBuf::from("a.rs"), PathBuf::from("b.py")];
        let f = ParsedFilter::default();
        let result = apply_filter(&paths, &f);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn apply_glob_double_star() {
        let paths = vec![
            PathBuf::from("src/lib.rs"),
            PathBuf::from("src/deep/mod.rs"),
            PathBuf::from("build.py"),
        ];
        let f = ParsedFilter {
            globs: vec!["**/*.rs".into()],
            ..Default::default()
        };
        let result = apply_filter(&paths, &f);
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|p| p.extension().unwrap() == "rs"));
    }

    #[test]
    fn apply_glob_single_star() {
        let paths = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("src/lib.py"),
            PathBuf::from("tests/test_main.rs"),
        ];
        // src/*.rs should match only files directly in src/
        let f = ParsedFilter {
            globs: vec!["src/*.rs".into()],
            ..Default::default()
        };
        let result = apply_filter(&paths, &f);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], PathBuf::from("src/main.rs"));
    }

    #[test]
    fn apply_glob_question_mark() {
        let paths = vec![
            PathBuf::from("a.rs"),
            PathBuf::from("ab.rs"),
            PathBuf::from("abc.rs"),
        ];
        // ?.rs matches exactly one character before the dot
        let f = ParsedFilter {
            globs: vec!["?.rs".into()],
            ..Default::default()
        };
        let result = apply_filter(&paths, &f);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], PathBuf::from("a.rs"));
    }

    #[test]
    fn build_glob_set_empty_returns_none() {
        assert!(build_glob_set(&[]).is_none());
    }

    #[test]
    fn build_glob_set_invalid_pattern_skipped() {
        // An invalid glob should not cause a panic
        let gs = build_glob_set(&["[invalid".into()]);
        // Either None (if build fails) or a GlobSet that matches nothing — either is safe
        if let Some(gs) = gs {
            assert!(!gs.is_match(std::path::Path::new("anything.rs")));
        }
    }
}
