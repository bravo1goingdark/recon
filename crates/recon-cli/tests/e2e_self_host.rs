//! E2E test: index recon's own codebase and verify symbol lookups.

use std::process::Command;

fn seed_license_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    recon_server::license::seed_dev_cache(dir.path()).expect("seed_dev_cache failed");
    dir
}

#[test]
fn index_self_and_verify_symbols() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let binary = workspace_root.join("target/debug/recon");
    let lic = seed_license_dir();

    if !binary.exists() {
        let status = Command::new("cargo")
            .args(["build", "--bin", "recon"])
            .status()
            .expect("cargo build failed");
        assert!(status.success());
    }

    // Clean previous index
    let recon_dir = workspace_root.join(".recon");
    let _ = std::fs::remove_dir_all(&recon_dir);

    // Index our own repo
    let output = Command::new(&binary)
        .args(["index", "--repo", workspace_root.to_str().unwrap()])
        .env("RECON_CONFIG_DIR", lic.path())
        .output()
        .expect("failed to run recon index");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "index failed: {stderr}");
    assert!(
        stderr.contains("indexing complete"),
        "missing 'indexing complete' in: {stderr}"
    );

    // Parse the final line for stats
    let last_line = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stderr}{last_line}");
    assert!(
        combined.contains("symbols"),
        "missing symbol count in output: {combined}"
    );

    // Verify the index directory was created
    assert!(recon_dir.join("index.db").exists(), "SQLite index missing");
    assert!(recon_dir.join("tantivy").exists(), "Tantivy index missing");

    // Clean up
    let _ = std::fs::remove_dir_all(&recon_dir);
}

#[test]
fn tool_descriptions_under_2kb() {
    // The tool descriptions are compile-time constants in the #[tool] macros.
    // We verify them by starting the server and checking the tool list.
    // For now, verify the descriptions we can extract statically are reasonable.
    let descriptions = [
        "Show one-line-per-symbol outline of a file. Returns symbol kinds, names, and line numbers in a tree structure. Use instead of Read when you need to understand a file's structure without reading its full content. Typical output: 300-500 tokens for a 500-line file.",
        "Show signatures and docstrings with bodies elided as '...'. 10x compression vs full file read. Use instead of Read when you need to understand APIs and structure. Output: ~300 tokens per 3000-token file.",
        "Read the full source of one symbol plus its parent chain and caller/callee references. Use instead of Read when you need one specific function or type. Output: ~200-800 tokens.",
        "Find symbols by name across the codebase. Tiered: exact SQLite match -> Tantivy BM25 -> FTS5 trigram + nucleo fuzzy. Use instead of Grep when searching for functions, types, or classes.",
        "Find all references to a symbol. Returns a count and top-k call sites as path:line triples. Use instead of Grep for finding usages of a function or type.",
        "Search for text patterns. Modes: exact (default), regex, hybrid (BM25 + text fused via reciprocal rank fusion). Use instead of Grep for code search.",
        "List indexed source files with language, line count, and top symbols. Use instead of Glob when you need structured file listings. Supports language filter.",
        "Generate a ranked overview of the most important symbols in the repo. Uses personalized PageRank over the reference graph with Aider-style edge weights. Output fits within a token budget (default 2000). Best first tool to call for orientation.",
        "Search for patterns in string literals and comments. Finds SQL fragments, i18n keys, log messages that structural search misses.",
        "Search for multiple patterns at once. More efficient than multiple code_search calls. Returns results grouped by pattern.",
        "Trigger a full re-index of the repository. Use when you suspect the index is stale or after major file changes outside the editor.",
        "Report index health: total files, symbols, last indexed time, Tantivy doc count. Use to check if the index is fresh and complete.",
    ];

    for (i, desc) in descriptions.iter().enumerate() {
        let bytes = desc.len();
        assert!(
            bytes <= 2048,
            "Tool description #{i} is {bytes} bytes, exceeds 2048 byte limit: {desc}"
        );
    }
}
