//! Per-repo baseline calibration (issue #29).
//!
//! Simulates the alternative Read+Grep loop an agent would run without
//! recon, per tool, against the actual indexed repo. Results are persisted
//! to SQLite `meta` so `Telemetry::baseline_for_local(tool)` returns a
//! repo-calibrated number instead of the static intel-repo estimate.
//!
//! Designed to run in a background `spawn_blocking` task after
//! `index_repo()` completes — never blocks the user's first tool call.

use ignore::WalkBuilder;
use recon_search::tokens::count_tokens;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

/// Maximum source files to include in the calibration run. 5K files
/// is statistically meaningful (within 5% of full-repo median on
/// benchmarks) and finishes in ~30s on typical hardware.
const MAX_CALIBRATION_FILES: usize = 5_000;

/// Maximum bytes per file — matches `MAX_READ_FILE_SIZE` in server.rs.
const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;

/// Source extensions included in calibration (same set as bench-baselines).
const EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "py", "go", "java", "c", "cpp", "h", "hpp",
];

/// Run calibration against `repo_root`, returning per-tool median token
/// counts for the non-migrated static-baseline tools. Capped at
/// [`MAX_CALIBRATION_FILES`] to bound runtime.
///
/// Returns a vec of `(tool_name, median_tokens)` pairs. Empty vec on
/// repos with no source files.
pub fn calibrate_baselines(repo_root: &Path) -> Vec<(&'static str, u64)> {
    let files = collect_source_files(repo_root);
    if files.is_empty() {
        return Vec::new();
    }
    debug!(files = files.len(), "calibration: collected source files");

    // Symbol inputs: pick real identifiers from the first few files.
    let symbols = extract_sample_identifiers(&files);
    if symbols.is_empty() {
        return Vec::new();
    }

    let mut results = Vec::new();

    // code_find_refs: grep for each symbol
    let find_refs = run_symbol_variants(&files, &symbols, alternative_find_refs);
    results.push(("code_find_refs", find_refs));

    // code_find_symbol: grep + read top 2 hits
    let find_sym =
        run_symbol_variants(&files, &symbols, |f, s| alternative_grep_then_read(f, s, 2));
    results.push(("code_find_symbol", find_sym));

    // code_repo_map: file envelope + read 5 files
    let repo_map = alternative_repo_map(&files);
    results.push(("code_repo_map", repo_map));

    // code_callers / code_callees: grep + read 1 hit
    let callers = run_symbol_variants(&files, &symbols, |f, s| alternative_grep_then_read(f, s, 1));
    results.push(("code_callers", callers));
    results.push(("code_callees", callers));

    // code_path: 5 chained find_refs
    let path_tokens = run_symbol_variants(&files, &symbols, |f, s| {
        // Simulate 5 hops with the same symbol (conservative floor)
        alternative_find_refs(f, s).saturating_mul(5)
    });
    results.push(("code_path", path_tokens));

    // code_impact: 3x transitive grep + test grep
    let impact = run_symbol_variants(&files, &symbols, alternative_impact);
    results.push(("code_impact", impact));

    // code_subsystems: repo_map + 5 extra file reads
    let subsystems = alternative_subsystems(&files);
    results.push(("code_subsystems", subsystems));

    // code_subsystem: directory listing + 4 file reads
    let subsystem = alternative_subsystem(&files);
    results.push(("code_subsystem", subsystem));

    info!(tools = results.len(), "calibration: complete");
    results
}

/// Collect source files from the repo, capped at [`MAX_CALIBRATION_FILES`].
fn collect_source_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .build();
    for entry in walker.flatten() {
        if out.len() >= MAX_CALIBRATION_FILES {
            break;
        }
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.into_path();
        let ext_ok = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| EXTENSIONS.contains(&e))
            .unwrap_or(false);
        if !ext_ok {
            continue;
        }
        if let Ok(meta) = path.metadata() {
            if meta.len() > MAX_READ_BYTES {
                continue;
            }
        }
        out.push(path);
    }
    out
}

/// Extract sample identifiers from the first few files for symbol-based
/// simulations. Picks function/struct/class names by scanning for common
/// definition patterns.
fn extract_sample_identifiers(files: &[PathBuf]) -> Vec<String> {
    let mut idents = Vec::new();
    let sample_files = files.len().min(20);
    for path in &files[..sample_files] {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for line in content.lines().take(200) {
            let trimmed = line.trim();
            // Extract identifiers after common definition keywords
            for prefix in &[
                "fn ",
                "struct ",
                "class ",
                "def ",
                "func ",
                "pub fn ",
                "pub struct ",
            ] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    let ident: String = rest
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if ident.len() >= 3 && idents.len() < 9 && !idents.contains(&ident) {
                        idents.push(ident);
                    }
                }
            }
        }
        if idents.len() >= 9 {
            break;
        }
    }
    // Fallback: if we couldn't find enough identifiers, use generic ones
    if idents.is_empty() {
        idents = vec!["main".into(), "new".into(), "from".into()];
    }
    idents
}

/// Run a simulation function across multiple symbol inputs and return the median.
fn run_symbol_variants<F>(files: &[PathBuf], symbols: &[String], mut f: F) -> u64
where
    F: FnMut(&[PathBuf], &str) -> u64,
{
    let mut samples: Vec<u64> = symbols.iter().map(|s| f(files, s)).collect();
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

// ── Alternative-loop simulators ──────────────────────────────────────────────

fn matches_word_boundary(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(rel) = haystack[start..].find(needle) {
        let abs = start + rel;
        let before_ok = abs == 0 || !is_word_byte(bytes[abs - 1]);
        let after = abs + needle.len();
        let after_ok = after == bytes.len() || !is_word_byte(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn alternative_find_refs(files: &[PathBuf], needle: &str) -> u64 {
    let mut output = String::new();
    for path in files {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (lineno, line) in content.lines().enumerate() {
            if matches_word_boundary(line, needle) {
                output.push_str(&format!("{}:{}:{}\n", path.display(), lineno + 1, line));
            }
        }
    }
    count_tokens(&output) as u64
}

fn alternative_grep_then_read(files: &[PathBuf], needle: &str, files_to_read: usize) -> u64 {
    let mut grep_output = String::new();
    let mut hit_files: Vec<&Path> = Vec::new();
    for path in files {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut file_hit = false;
        for (lineno, line) in content.lines().enumerate() {
            if matches_word_boundary(line, needle) {
                grep_output.push_str(&format!("{}:{}:{}\n", path.display(), lineno + 1, line));
                file_hit = true;
            }
        }
        if file_hit {
            hit_files.push(path);
        }
    }
    let mut total = count_tokens(&grep_output) as u64;
    for path in hit_files.iter().take(files_to_read) {
        if let Ok(content) = std::fs::read_to_string(path) {
            total = total.saturating_add(count_tokens(&content) as u64);
        }
    }
    total
}

fn alternative_repo_map(files: &[PathBuf]) -> u64 {
    let mut envelope = String::new();
    for path in files {
        envelope.push_str(&format!("{}\n", path.display()));
    }
    let mut total = count_tokens(&envelope) as u64;
    for path in files.iter().take(5) {
        if let Ok(content) = std::fs::read_to_string(path) {
            total = total.saturating_add(count_tokens(&content) as u64);
        }
    }
    total
}

fn alternative_impact(files: &[PathBuf], symbol: &str) -> u64 {
    let mut total = 0u64;
    for _ in 0..3 {
        total = total.saturating_add(alternative_find_refs(files, symbol));
    }
    let test_files: Vec<&Path> = files
        .iter()
        .filter(|p| p.to_string_lossy().to_lowercase().contains("test"))
        .map(|p| p.as_path())
        .collect();
    let mut test_grep = String::new();
    for path in test_files {
        if let Ok(content) = std::fs::read_to_string(path) {
            for (lineno, line) in content.lines().enumerate() {
                if matches_word_boundary(line, symbol) {
                    test_grep.push_str(&format!("{}:{}:{}\n", path.display(), lineno + 1, line));
                }
            }
        }
    }
    total.saturating_add(count_tokens(&test_grep) as u64)
}

fn alternative_subsystems(files: &[PathBuf]) -> u64 {
    let mut total = alternative_repo_map(files);
    for path in files.iter().skip(5).take(5) {
        if let Ok(content) = std::fs::read_to_string(path) {
            total = total.saturating_add(count_tokens(&content) as u64);
        }
    }
    total
}

fn alternative_subsystem(files: &[PathBuf]) -> u64 {
    if files.is_empty() {
        return 0;
    }
    let first_dir = files[0].parent();
    let in_dir: Vec<&Path> = files
        .iter()
        .filter(|p| p.parent() == first_dir)
        .map(|p| p.as_path())
        .collect();
    let mut envelope = String::new();
    for p in &in_dir {
        envelope.push_str(&format!("{}\n", p.display()));
    }
    let mut total = count_tokens(&envelope) as u64;
    for path in in_dir.iter().take(4) {
        if let Ok(content) = std::fs::read_to_string(path) {
            total = total.saturating_add(count_tokens(&content) as u64);
        }
    }
    total
}
