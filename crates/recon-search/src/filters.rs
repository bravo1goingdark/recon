//! Search filter DSL powered by `fff-query-parser`.
//!
//! Parses constraint strings like `"*.rs"`, `"status:modified"`, `"!test"`,
//! `"size:>1mb"`, `"mtime:<2d"` and applies them to narrow file scope before text search.

use globset::{Glob, GlobSet, GlobSetBuilder};
use recon_core::error::Error;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    /// Minimum file size in bytes (inclusive).
    pub size_min_bytes: Option<u64>,
    /// Maximum file size in bytes (inclusive).
    pub size_max_bytes: Option<u64>,
    /// Earest acceptable modification time as Unix epoch seconds.
    pub mtime_after: Option<u64>,
    /// Latest acceptable modification time as Unix epoch seconds.
    pub mtime_before: Option<u64>,
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
            fff_query_parser::Constraint::Text(t) => {
                parse_pseudo_constraint(t, &mut parsed);
            }
            _ => {
                // Parts, Exclude, other git statuses, FilePath — skip for now
            }
        }
    }

    // Also check fuzzy query parts for size:/mtime: pseudo-constraints
    match &result.fuzzy_query {
        fff_query_parser::FuzzyQuery::Text(t) => {
            parse_pseudo_constraint(t, &mut parsed);
        }
        fff_query_parser::FuzzyQuery::Parts(parts) => {
            for part in parts {
                parse_pseudo_constraint(part, &mut parsed);
            }
        }
        fff_query_parser::FuzzyQuery::Empty => {}
    }

    Ok(parsed)
}

/// Check a text token for size:/mtime: pseudo-constraints.
fn parse_pseudo_constraint(token: &str, parsed: &mut ParsedFilter) {
    if let Some((key, value)) = token.split_once(':') {
        match key {
            "size" => {
                if let Ok(bytes) = parse_size_value(value) {
                    if value.starts_with('>') {
                        parsed.size_min_bytes = Some(bytes);
                    } else if value.starts_with('<') {
                        parsed.size_max_bytes = Some(bytes);
                    }
                }
            }
            "mtime" => {
                if let Ok(epoch) = parse_mtime_value(value) {
                    if value.starts_with('>') {
                        parsed.mtime_after = Some(epoch);
                    } else if value.starts_with('<') {
                        parsed.mtime_before = Some(epoch);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Parse a size value like `>1mb`, `<50kb`, `>100`, `<1gb` into bytes.
fn parse_size_value(value: &str) -> Result<u64, ()> {
    let trimmed = value.trim_start_matches(['>', '<', '=']);
    let trimmed = trimmed.trim();

    let multiplier = if trimmed.ends_with("gb") || trimmed.ends_with("GB") {
        1_073_741_824u64
    } else if trimmed.ends_with("mb") || trimmed.ends_with("MB") {
        1_048_576u64
    } else if trimmed.ends_with("kb") || trimmed.ends_with("KB") {
        1_024u64
    } else {
        1u64
    };

    let num_str = if multiplier > 1 {
        &trimmed[..trimmed.len() - 2]
    } else {
        trimmed
    };

    let num: u64 = num_str.parse().map_err(|_| ())?;
    num.checked_mul(multiplier).ok_or(())
}

/// Parse an mtime value like `>2d`, `<1h`, `>30m`, `<7d` into Unix epoch seconds.
fn parse_mtime_value(value: &str) -> Result<u64, ()> {
    let trimmed = value.trim_start_matches(['>', '<', '=']);
    let trimmed = trimmed.trim();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ())?
        .as_secs();

    let seconds = if trimmed.ends_with('d') || trimmed.ends_with('D') {
        let num: u64 = trimmed[..trimmed.len() - 1].parse().map_err(|_| ())?;
        num * 86_400
    } else if trimmed.ends_with('h') || trimmed.ends_with('H') {
        let num: u64 = trimmed[..trimmed.len() - 1].parse().map_err(|_| ())?;
        num * 3_600
    } else if trimmed.ends_with('m') || trimmed.ends_with('M') {
        let num: u64 = trimmed[..trimmed.len() - 1].parse().map_err(|_| ())?;
        num * 60
    } else {
        // Treat as absolute Unix timestamp
        trimmed.parse().map_err(|_| ())?
    };

    if value.starts_with('>') {
        now.checked_sub(seconds).ok_or(())
    } else {
        now.checked_add(seconds).ok_or(())
    }
}

/// Apply parsed filter constraints to a list of file paths.
///
/// Returns only paths that match all constraints.
///
/// When `git_modified_paths` is provided (non-empty slice), `git_modified_only`
/// is enforced by intersecting with that set. The caller is responsible for
/// resolving git-modified paths via `recon_indexer::git::status_paths`.
pub fn apply_filter(
    paths: &[PathBuf],
    filter: &ParsedFilter,
    git_modified_paths: Option<&[PathBuf]>,
) -> Vec<PathBuf> {
    // Resolve git-modified set once if needed
    let git_set: Option<std::collections::HashSet<&PathBuf>> = if filter.git_modified_only {
        git_modified_paths.map(|p| p.iter().collect())
    } else {
        None
    };

    // Compile glob patterns once for the whole batch
    let glob_set = build_glob_set(&filter.globs);

    paths
        .iter()
        .filter(|p| {
            // Git-modified intersection check
            if filter.git_modified_only {
                if let Some(ref set) = git_set {
                    if !set.contains(p) {
                        return false;
                    }
                } else {
                    // git_modified_only requested but no paths provided — exclude all
                    return false;
                }
            }
            matches_filter(p, filter, glob_set.as_ref())
        })
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

    // Size filter — requires filesystem access
    if filter.size_min_bytes.is_some() || filter.size_max_bytes.is_some() {
        if let Ok(meta) = path.metadata() {
            let size = meta.len();
            if let Some(min) = filter.size_min_bytes {
                if size < min {
                    return false;
                }
            }
            if let Some(max) = filter.size_max_bytes {
                if size > max {
                    return false;
                }
            }
        } else {
            return false;
        }
    }

    // Mtime filter — requires filesystem access
    if filter.mtime_after.is_some() || filter.mtime_before.is_some() {
        if let Ok(meta) = path.metadata() {
            if let Ok(modified) = meta.modified() {
                if let Ok(epoch) = modified.duration_since(UNIX_EPOCH) {
                    let secs = epoch.as_secs();
                    if let Some(after) = filter.mtime_after {
                        if secs < after {
                            return false;
                        }
                    }
                    if let Some(before) = filter.mtime_before {
                        if secs > before {
                            return false;
                        }
                    }
                } else {
                    return false;
                }
            } else {
                return false;
            }
        } else {
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
    fn parse_size_greater_than() {
        let f = parse_filter("size:>1mb foo").unwrap();
        assert_eq!(f.size_min_bytes, Some(1_048_576));
        assert!(f.size_max_bytes.is_none());
    }

    #[test]
    fn parse_size_less_than() {
        let f = parse_filter("size:<50kb foo").unwrap();
        assert_eq!(f.size_max_bytes, Some(51_200));
        assert!(f.size_min_bytes.is_none());
    }

    #[test]
    fn parse_size_plain_bytes() {
        let f = parse_filter("size:>100 foo").unwrap();
        assert_eq!(f.size_min_bytes, Some(100));
    }

    #[test]
    fn parse_size_gigabytes() {
        let f = parse_filter("size:<2gb foo").unwrap();
        assert_eq!(f.size_max_bytes, Some(2_147_483_648));
    }

    #[test]
    fn parse_mtime_greater_than() {
        let f = parse_filter("mtime:>2d foo").unwrap();
        assert!(f.mtime_after.is_some());
        assert!(f.mtime_before.is_none());
    }

    #[test]
    fn parse_mtime_less_than() {
        let f = parse_filter("mtime:<1h foo").unwrap();
        assert!(f.mtime_before.is_some());
        assert!(f.mtime_after.is_none());
    }

    #[test]
    fn parse_mtime_minutes() {
        let f = parse_filter("mtime:>30m foo").unwrap();
        assert!(f.mtime_after.is_some());
    }

    #[test]
    fn parse_size_invalid_returns_none() {
        let f = parse_filter("size:abc foo").unwrap();
        assert!(f.size_min_bytes.is_none());
        assert!(f.size_max_bytes.is_none());
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
        let result = apply_filter(&paths, &f, None);
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
        let result = apply_filter(&paths, &f, None);
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
        let result = apply_filter(&paths, &f, None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], PathBuf::from("main.rs"));
    }

    #[test]
    fn empty_filter_passes_all() {
        let paths = vec![PathBuf::from("a.rs"), PathBuf::from("b.py")];
        let f = ParsedFilter::default();
        let result = apply_filter(&paths, &f, None);
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
        let result = apply_filter(&paths, &f, None);
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
        let result = apply_filter(&paths, &f, None);
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
        let result = apply_filter(&paths, &f, None);
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

    #[test]
    fn git_modified_only_filters_to_intersection() {
        let paths = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("src/lib.rs"),
            PathBuf::from("src/util.rs"),
        ];
        let modified = vec![PathBuf::from("src/main.rs"), PathBuf::from("src/util.rs")];
        let f = ParsedFilter {
            git_modified_only: true,
            ..Default::default()
        };
        let result = apply_filter(&paths, &f, Some(&modified));
        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|p| p.ends_with("main.rs")));
        assert!(result.iter().any(|p| p.ends_with("util.rs")));
        assert!(!result.iter().any(|p| p.ends_with("lib.rs")));
    }

    #[test]
    fn git_modified_only_no_paths_excludes_all() {
        let paths = vec![PathBuf::from("src/main.rs")];
        let f = ParsedFilter {
            git_modified_only: true,
            ..Default::default()
        };
        let result = apply_filter(&paths, &f, None);
        assert!(result.is_empty());
    }

    #[test]
    fn git_modified_only_false_ignores_paths() {
        let paths = vec![PathBuf::from("src/main.rs")];
        let f = ParsedFilter {
            git_modified_only: false,
            ..Default::default()
        };
        // Even with git_modified_paths provided, should pass through
        let modified = vec![PathBuf::from("other.rs")];
        let result = apply_filter(&paths, &f, Some(&modified));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn size_filter_excludes_small_files() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.rs");
        std::fs::write(&small, "x").unwrap(); // 1 byte
        let big = dir.path().join("big.rs");
        std::fs::write(&big, "x".repeat(2000)).unwrap(); // 2000 bytes

        let f = ParsedFilter {
            size_min_bytes: Some(100),
            ..Default::default()
        };
        let result = apply_filter(&[small, big], &f, None);
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("big.rs"));
    }

    #[test]
    fn size_filter_excludes_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.rs");
        std::fs::write(&small, "x").unwrap();
        let big = dir.path().join("big.rs");
        std::fs::write(&big, "x".repeat(2000)).unwrap();

        let f = ParsedFilter {
            size_max_bytes: Some(100),
            ..Default::default()
        };
        let result = apply_filter(&[small, big], &f, None);
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("small.rs"));
    }

    #[test]
    fn mtime_filter_excludes_old_files() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.rs");
        std::fs::write(&old, "old content").unwrap();
        // Set mtime to 10 days ago
        let ten_days_ago = SystemTime::now() - std::time::Duration::from_secs(864_000);
        filetime::set_file_mtime(&old, filetime::FileTime::from_system_time(ten_days_ago)).unwrap();

        let new = dir.path().join("new.rs");
        std::fs::write(&new, "new content").unwrap();

        // mtime:>1d should exclude the old file
        let f = parse_filter("mtime:>1d").unwrap();
        let result = apply_filter(&[old, new], &f, None);
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("new.rs"));
    }

    #[test]
    fn combined_filters_all_must_match() {
        let dir = tempfile::tempdir().unwrap();
        let matching = dir.path().join("main.rs");
        std::fs::write(&matching, "x".repeat(500)).unwrap();
        let wrong_ext = dir.path().join("main.py");
        std::fs::write(&wrong_ext, "x".repeat(500)).unwrap();
        let too_small = dir.path().join("tiny.rs");
        std::fs::write(&too_small, "x").unwrap();

        let f = ParsedFilter {
            extensions: vec!["rs".into()],
            size_min_bytes: Some(100),
            ..Default::default()
        };
        let result = apply_filter(&[matching, wrong_ext, too_small], &f, None);
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("main.rs"));
    }

    #[test]
    fn parse_size_value_kb() {
        assert_eq!(parse_size_value(">10kb").unwrap(), 10_240);
        assert_eq!(parse_size_value("<5KB").unwrap(), 5_120);
    }

    #[test]
    fn parse_size_value_mb() {
        assert_eq!(parse_size_value(">2mb").unwrap(), 2_097_152);
    }

    #[test]
    fn parse_size_value_gb() {
        assert_eq!(parse_size_value("<1gb").unwrap(), 1_073_741_824);
    }

    #[test]
    fn parse_size_value_plain() {
        assert_eq!(parse_size_value(">500").unwrap(), 500);
    }

    #[test]
    fn parse_size_value_invalid() {
        assert!(parse_size_value(">abc").is_err());
        assert!(parse_size_value("<").is_err());
    }

    #[test]
    fn parse_mtime_value_days() {
        let result = parse_mtime_value(">2d").unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Should be roughly 2 days ago (within 10 seconds for test timing)
        assert!(now - result >= 172_700 && now - result <= 172_900);
    }

    #[test]
    fn parse_mtime_value_hours() {
        let result = parse_mtime_value("<3h").unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Should be roughly 3 hours in the future
        assert!(result - now >= 10_700 && result - now <= 10_900);
    }

    #[test]
    fn parse_mtime_value_minutes() {
        let result = parse_mtime_value(">30m").unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(now - result >= 1_790 && now - result <= 1_810);
    }

    #[test]
    fn parse_mtime_value_invalid() {
        assert!(parse_mtime_value(">abc").is_err());
        assert!(parse_mtime_value("<").is_err());
    }
}
