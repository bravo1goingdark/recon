//! E2E test: index a representative copy of recon's own source and
//! verify symbol lookups.
//!
//! **Why we don't index `workspace_root` directly any more.** Up to v0.3.3
//! this test ran `recon index --repo <workspace_root>` and wiped
//! `<workspace_root>/.recon` on entry and exit. That's destructive when
//! a long-running `recon serve` (e.g. spawned by Claude Code / Cursor /
//! Windsurf against the same checkout) is currently using that index dir
//! — the test unlinks the inode while the running process holds open
//! file descriptors to it, leaving the server writing into a phantom
//! database the on-disk filesystem can't see, until SIGKILL.
//!
//! v0.3.4 fix: copy a representative subset of the workspace into a
//! `tempfile::TempDir`, point `recon index` at that, and let the
//! drop-handler clean up. The test still exercises the real binary
//! against real source code — just not the user's live source code.

use std::path::Path;
use std::process::Command;

fn seed_license_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    recon_server::license::seed_dev_cache(dir.path()).expect("seed_dev_cache failed");
    dir
}

/// Copy a directory tree recursively. Symlinks are followed (we want
/// the real file content); skips entries whose name appears in
/// `skip_names` so we don't pull `target/`, `.recon/`, `.git/`, etc.
fn copy_tree(src: &Path, dst: &Path, skip_names: &[&str]) {
    std::fs::create_dir_all(dst).expect("create dst dir");
    let entries = std::fs::read_dir(src).expect("read_dir src");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if skip_names.iter().any(|s| *s == name_str) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let ft = entry.file_type().expect("file_type");
        if ft.is_dir() {
            copy_tree(&from, &to, skip_names);
        } else {
            std::fs::copy(&from, &to).expect("copy file");
        }
    }
}

/// Materialise a self-contained "mini-workspace" snapshot in a tempdir
/// that's enough to exercise multi-crate indexing. Returns the tempdir
/// (kept alive via Drop until the test finishes).
fn build_isolated_workspace_snapshot() -> tempfile::TempDir {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let snap = tempfile::tempdir().expect("snapshot tempdir");
    let dst = snap.path();

    // Top-level workspace anchor + license + readme.
    for f in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"] {
        let from = workspace_root.join(f);
        if from.exists() {
            std::fs::copy(&from, dst.join(f)).expect("copy top-level file");
        }
    }

    // Two representative crates — small enough to copy fast, varied
    // enough to exercise cross-crate indexing.
    let skip = &[
        "target",
        ".recon",
        ".git",
        "node_modules",
        ".idea",
        ".vscode",
    ];
    let crates_dst = dst.join("crates");
    std::fs::create_dir_all(&crates_dst).expect("create crates dir");
    for c in ["recon-core", "recon-storage"] {
        let from = workspace_root.join("crates").join(c);
        if from.exists() {
            copy_tree(&from, &crates_dst.join(c), skip);
        }
    }
    snap
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

    // Snapshot: isolated tempdir copy of representative source. The
    // tempdir's `.recon/` is the one we wipe; the live workspace's
    // is never touched.
    let snap = build_isolated_workspace_snapshot();
    let snap_root = snap.path();
    let recon_dir = snap_root.join(".recon");
    let _ = std::fs::remove_dir_all(&recon_dir);

    let output = Command::new(&binary)
        .args(["index", "--repo", snap_root.to_str().unwrap()])
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

    // Verify the index directory was created inside the snapshot — and
    // that the live workspace `.recon/` is untouched (regression guard
    // for the v0.3.3 test-isolation bug).
    assert!(
        recon_dir.join("index.db").exists(),
        "SQLite index missing in snapshot"
    );
    assert!(
        recon_dir.join("tantivy").exists(),
        "Tantivy index missing in snapshot"
    );
    assert_ne!(
        recon_dir,
        workspace_root.join(".recon"),
        "snapshot recon_dir must not equal workspace .recon"
    );

    // tempdir auto-cleans on drop; no manual remove_dir_all needed.
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
        // v0.3.0 graph-traversal tools.
        "Shortest call-graph path from `src` to `dst`. Use to answer 'how does X reach Y?' — replaces a chain of code_find_refs calls. Both arguments accept a bare name or a fully qualified name (preferred — disambiguates). Returns an ordered hop sequence with file:line per hop. When unreachable within `max_hops` (default 8, max 16) returns an Error with kind 'unreachable' plus an `unresolved_hint` when the BFS hit a likely dyn-dispatch / FFI boundary. When src or dst is ambiguous (multiple symbols share the name) the BFS spans the cross-product and returns the shortest match. Bidirectional BFS over the cached reference graph; total-visit cap 50 000 nodes. Output uses ReferenceDigest with the `path` field populated.",
        "Transitive callers of `symbol` up to `depth` rings (default 1, max 6). Replaces depth-many chained code_find_refs calls. Returns one tier per ring with the symbols at that depth. Cycle-safe (each symbol emitted at its minimum depth only). Per-tier fan-out is capped at 50 to bound god-node responses; total-visit cap 50 000 nodes. When either cap fires `truncated: true` is set. Returns symbol identities (qname + path + line of definition), not call-site lines — use code_find_refs for the lexical call-site digest. `symbol` accepts bare or fully qualified names; ambiguous bare names traverse from all matches. Output uses ReferenceDigest with the `tiers` field populated.",
        "Transitive callees of `symbol` up to `depth` rings (default 1, max 6). Mirror of code_callers — what does this symbol call (directly and transitively)? Cycle-safe, per-tier fan-out capped at 50, total-visit cap 50 000. `truncated: true` when caps fire. Returns symbol identities (qname + path + line of definition), not call-site lines. Use this to understand what changing X *requires* you to also understand (callees) versus what changing X *risks breaking* (callers). Output uses ReferenceDigest with the `tiers` field populated.",
        "One-shot bundle of everything an agent needs to reason about a symbol — replaces the canonical 4-call understand-X loop (find_symbol → read_symbol → find_refs → search-for-tests). Returns: (1) the target symbol's signature + doc + first ~20 body lines, (2) up to 5 immediate callers, (3) up to 5 immediate callees, (4) up to 3 referenced types, (5) up to 3 tests that exercise it. Honors `token_budget` (default 2000); drops sections under pressure in this order: tests → callees → types → callers (skeleton+body always kept). Accepts a bare name or a fully qualified name. When ambiguous (multiple symbols share the bare name) returns an Error with kind 'invalid_params' listing up to 5 candidates; reissue with a qualified name. Output uses SymbolCard with the `context` envelope populated. Test detection in v0.3 is Rust-only (tests::* qname patterns and test_* / Test* function names); Phase 2 adds cross-language coverage.",
        // v0.3.0 Phase-2 graph tools.
        "Blast radius of changing `symbol` — transitive callers up to `depth` rings (default 4, max 6) plus tests that exercise it. Returns one tier per ring (production callers), a separate `tests` array for transitively-reaching test functions (Rust-only Phase-1 detector: tests::* qnames + test_* / Test* names), and `truncated: true` when fan-out caps fire. Use to answer 'what might break if I change X?' before refactoring. Per-tier fan-out cap 50, total-visit cap 50 000 — a god-node query terminates with a marker rather than blowing up. Output uses ReferenceDigest with the `tiers` and `tests` fields populated.",
        "List the natural subsystems of the repo — weakly-connected components of the reference graph. Each subsystem has an id (use with code_subsystem), the qualified-name of its highest-degree symbol (the 'hub'), the dominant directory, and a symbol count. Use to orient yourself before drilling in: subsystems separate cleanly along architectural lines (e.g. recon-search vs recon-storage) without you having to know the directory structure. Sorted by symbol count descending. `limit` caps the number returned (default 50). Output uses Skeleton with subsystems rendered as one line each. Phase 2 v0.3.x: connected components only. Future v0.4.x adds Leiden modularity-optimized clustering.",
        "Detailed view of one subsystem (from code_subsystems). Returns a skeleton-style summary of all symbols in the component — qname, kind, file:line — within `token_budget` tokens (default 1500). Use after code_subsystems to drill into a specific cluster without reading every file in the directory. Output uses Skeleton.",
        // v0.3.1 telemetry surface.
        "Per-tool token-savings counter. Returns a tab-separated breakdown: tool, calls, response_tokens_emitted, baseline_tokens_avoided, tokens_saved, avg_latency_ms — plus an aggregate trailer. Lifetime totals persist across restarts via the meta table; session totals reset every server start. recon is model-agnostic, so we report tokens; convert against your provider's price sheet (Claude, GPT, Gemini, local — your rates, your math). Output uses Skeleton (header + one row per tool).",
    ];

    for (i, desc) in descriptions.iter().enumerate() {
        let bytes = desc.len();
        assert!(
            bytes <= 2048,
            "Tool description #{i} is {bytes} bytes, exceeds 2048 byte limit: {desc}"
        );
    }
}
