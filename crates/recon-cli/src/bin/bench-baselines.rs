//! `bench-baselines` — derive the static `BASELINES` table from
//! measured Read+Grep workload runs against a real repository.
//!
//! Until v0.5.4 the static-baseline rows in
//! `recon_server::telemetry::BASELINES` carried point estimates that
//! came from rationale-based math (e.g. `code_repo_map = 20 000 tok
//! ≈ 5 files × 4 KB / 4`). Defensible per-row but not auditable —
//! a skeptical reviewer asks "where did 20 000 come from?" and the
//! answer is "I made it up but here's the reasoning." Closing that
//! gap is what this binary does.
//!
//! For each non-migrated tool, we simulate the literal alternative
//! the agent would do without recon — `grep` across the repo, read
//! the top-N hit files, etc. — and BPE-count the resulting output
//! exactly the way `count_tokens` counts a measured baseline. Each
//! tool runs across multiple input variants so we report a band
//! (low / median / high), not a single integer pretending to be
//! exact. The output is a Rust source snippet ready to drop into
//! `BASELINES`.
//!
//! ## Reproduction
//!
//! ```sh
//! RECON_LICENSE_HMAC_KEY=bench-dev-only cargo run --release \
//!     -p recon-cli --bin bench-baselines
//! ```
//!
//! Run from the repo root or pass `--repo <path>` to point at a
//! different fixture. Output goes to stdout; pipe into
//! `crates/recon-server/src/telemetry.rs` after review.

use ignore::WalkBuilder;
use recon_search::tokens::count_tokens;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Maximum bytes to read per file when simulating a `Read` call.
/// Matches the cap real handlers apply (see `MAX_READ_FILE_SIZE` in
/// recon-server). Files larger than this don't accrue baseline.
const MAX_READ_BYTES: u64 = 5 * 1024 * 1024;

/// Outcome of one tool's alternative-loop simulation across N input
/// variants. Token counts are real BPE counts via `count_tokens`;
/// latency is wall-clock time of the simulator (NOT a substitute
/// for the static `baseline_latency_ms` — agents are slower than
/// our local in-process sim — but useful as an order-of-magnitude
/// floor).
#[derive(Debug, Clone)]
struct Measurement {
    tool: &'static str,
    samples: Vec<u64>,
    latency_micros_total: u64,
}

impl Measurement {
    fn new(tool: &'static str) -> Self {
        Self {
            tool,
            samples: Vec::new(),
            latency_micros_total: 0,
        }
    }

    fn record_run(&mut self, tokens: u64, latency_us: u64) {
        self.samples.push(tokens);
        self.latency_micros_total = self.latency_micros_total.saturating_add(latency_us);
    }

    fn low(&self) -> u64 {
        self.samples.iter().copied().min().unwrap_or(0)
    }

    fn median(&self) -> u64 {
        if self.samples.is_empty() {
            return 0;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }

    fn high(&self) -> u64 {
        self.samples.iter().copied().max().unwrap_or(0)
    }

    fn avg_latency_ms(&self) -> u64 {
        if self.samples.is_empty() {
            return 0;
        }
        // Round to nearest ms; over-precision here is dishonest —
        // the simulated latency is bounded by local I/O speed and
        // doesn't reflect agent round-trip time anyway.
        ((self.latency_micros_total / self.samples.len() as u64) + 500) / 1000
    }
}

/// Hand-rolled word-boundary search — same semantics as `\b<name>\b`
/// in regex. Avoids pulling in the `regex` crate as a new workspace
/// dependency for a one-off bench. ASCII-only word characters
/// (`[a-zA-Z0-9_]`); identifier-name search in source code is the
/// only use case here, so no Unicode-aware boundary needed.
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

/// Walk the fixture repo respecting .gitignore + .ignore. Filters to
/// extensions a real agent would Read — `.rs`, `.ts`, `.tsx`, `.js`,
/// `.py`, `.go`, `.java`, `.c`, `.cpp`, `.md`. Bench output is
/// repo-shape-dependent; the band we report captures that.
fn collect_source_files(root: &Path) -> Vec<PathBuf> {
    const EXTENSIONS: &[&str] = &[
        "rs", "ts", "tsx", "js", "py", "go", "java", "c", "cpp", "h", "hpp", "md",
    ];
    let mut out = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .build();
    for entry in walker.flatten() {
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

/// Simulate a single grep run: every file, every line, word-boundary
/// match. Returns the aggregate "what the agent would have read" —
/// the literal `path:line:matched_line\n` lines a `grep -rn` invocation
/// would emit. BPE-counted, then summed with the contents of the top-N
/// hit files (typical agent flow: read the grep, then `Read` the most
/// promising hits to confirm).
fn alternative_grep_then_read(files: &[PathBuf], needle: &str, files_to_read_after: usize) -> u64 {
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
    // Read top-N hit files. "Top" here = the first N in walk order;
    // a real agent would prioritize by relevance but we don't have
    // a free relevance signal in the simulator, and over-reading
    // overstates the baseline (worse for our credibility), which
    // is the wrong direction. First-N is a reasonable floor.
    for path in hit_files.iter().take(files_to_read_after) {
        if let Ok(content) = std::fs::read_to_string(path) {
            total = total.saturating_add(count_tokens(&content) as u64);
        }
    }
    total
}

/// Simulate `code_find_refs` alternative: grep across the repo for
/// the symbol name. No file reads — the agent uses the grep output
/// as the answer. (The static-only rationale doc says the same:
/// "Grep for symbol name across repo".)
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

/// Simulate `code_find_symbol` alternative: grep for the symbol's
/// definition pattern + read the top 2 hit files for confirmation.
fn alternative_find_symbol(files: &[PathBuf], symbol: &str) -> u64 {
    alternative_grep_then_read(files, symbol, 2)
}

/// Simulate `code_repo_map` alternative: list all files (envelope)
/// plus read the 5 most "central" (first-in-walk) files for
/// orientation. Captures both halves of the documented rationale
/// "Read 5 files for orientation".
fn alternative_repo_map(files: &[PathBuf]) -> u64 {
    // Envelope: the agent runs `find . -type f` or equivalent, the
    // output is one path per line. BPE-count is dominated by paths.
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

/// Simulate `code_callers` / `code_callees` alternative: depth-1
/// reference grep + read of the *target's* file (callees) or top
/// hit (callers). Both reduce to a `find_refs`-shape with one
/// extra file read for context.
fn alternative_callers_or_callees(files: &[PathBuf], needle: &str) -> u64 {
    alternative_grep_then_read(files, needle, 1)
}

/// Simulate `code_path` alternative: 5 chained `find_refs` calls
/// for a hypothetical from→to traversal. Pessimistic: real agents
/// often shortcut once a path is found, but the alternative-loop
/// rationale says "5x chained code_find_refs", so we honor that.
fn alternative_path(files: &[PathBuf], chain: &[&str]) -> u64 {
    let mut total = 0u64;
    for needle in chain {
        total = total.saturating_add(alternative_find_refs(files, needle));
    }
    total
}

/// Simulate `code_impact` alternative: 3 chained ref greps
/// (transitive callers approximation) plus a grep for test files
/// matching the symbol.
fn alternative_impact(files: &[PathBuf], symbol: &str) -> u64 {
    let mut total = 0u64;
    // 3x transitive grep (depth=3 caller chain)
    for _ in 0..3 {
        total = total.saturating_add(alternative_find_refs(files, symbol));
    }
    // Test detection: list the test files relevant to the symbol.
    // Heuristic: any file path containing "test" + a final grep for
    // the symbol in each.
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
    total = total.saturating_add(count_tokens(&test_grep) as u64);
    total
}

/// Simulate `code_subsystems` alternative: a `repo_map`-equivalent
/// orientation pass plus 5 file reads to ground the connected-
/// component story. Roughly `repo_map + 5 file reads` per the
/// rationale.
fn alternative_subsystems(files: &[PathBuf]) -> u64 {
    let mut total = alternative_repo_map(files);
    for path in files.iter().skip(5).take(5) {
        if let Ok(content) = std::fs::read_to_string(path) {
            total = total.saturating_add(count_tokens(&content) as u64);
        }
    }
    total
}

/// Simulate `code_subsystem` alternative: directory listing + read
/// of the top 4 files in that subsystem. We approximate by
/// truncating the file list to the first directory's worth and
/// reading 4 from it.
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

/// Run a closure across `inputs` and accumulate the per-input token
/// counts and wall times into a `Measurement`.
fn run_variants<F: FnMut(&str) -> u64>(
    tool: &'static str,
    inputs: &[&'static str],
    mut runner: F,
) -> Measurement {
    let mut m = Measurement::new(tool);
    for input in inputs {
        let started = Instant::now();
        let tokens = runner(input);
        let latency_us = started.elapsed().as_micros() as u64;
        m.record_run(tokens, latency_us);
    }
    m
}

/// One-shot variant for tools whose alternative loop doesn't take
/// an input parameter (e.g. `code_repo_map`).
#[allow(dead_code)]
fn run_oneshot<F: FnOnce() -> u64>(tool: &'static str, runner: F) -> Measurement {
    let mut m = Measurement::new(tool);
    let started = Instant::now();
    let tokens = runner();
    let latency_us = started.elapsed().as_micros() as u64;
    m.record_run(tokens, latency_us);
    m
}

/// Repo-shape variant: runs the closure across multiple cuts of the
/// file list (a third, two thirds, all) so a tool whose alternative
/// loop doesn't take a parameter still produces a meaningful low /
/// median / high band. Captures "how does this tool's baseline
/// scale with repo size" — the most important shape variance for
/// repo-orientation tools.
fn run_on_subsets<F: Fn(&[PathBuf]) -> u64>(
    tool: &'static str,
    files: &[PathBuf],
    runner: F,
) -> Measurement {
    let mut m = Measurement::new(tool);
    if files.is_empty() {
        return m;
    }
    let n = files.len();
    let cuts = [n / 3, (2 * n) / 3, n];
    for cut in cuts {
        if cut == 0 {
            continue;
        }
        let subset = &files[..cut];
        let started = Instant::now();
        let tokens = runner(subset);
        let latency_us = started.elapsed().as_micros() as u64;
        m.record_run(tokens, latency_us);
    }
    m
}

/// Format a Measurement as a `Baseline { … }` Rust struct literal,
/// ready to drop into the `BASELINES` table. Only the
/// `baseline_tokens` / `range_low_tokens` / `range_high_tokens` /
/// `baseline_latency_ms` are sourced from the measurement —
/// rationale and derivation strings are left as `"…"` placeholders
/// for the reviewer to fill in (the bench can't produce prose).
fn print_baseline_literal(m: &Measurement) {
    println!(
        "    Baseline {{
        tool: {tool:?},
        baseline_tokens: {median},
        range_low_tokens: {low},
        range_high_tokens: {high},
        baseline_latency_ms: {latency},
        rationale: \"…\",
        derivation: \"measured against intel repo via bench-baselines on $(date -u +%Y-%m-%d)\",
    }},",
        tool = m.tool,
        median = m.median(),
        low = m.low(),
        high = m.high(),
        latency = m.avg_latency_ms(),
    );
}

fn main() {
    let repo_root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // Default: walk up from CARGO_MANIFEST_DIR to the
            // workspace root. The bench was authored against intel
            // (this repo); if you point it elsewhere via CLI arg,
            // the derived numbers reflect that fixture's shape.
            let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
            let mut p = PathBuf::from(manifest);
            // crates/recon-cli → crates → root
            for _ in 0..2 {
                if let Some(parent) = p.parent() {
                    p = parent.to_path_buf();
                }
            }
            p
        });

    eprintln!("bench-baselines: walking {}", repo_root.display());
    let files = collect_source_files(&repo_root);
    eprintln!("bench-baselines: {} source files in scope", files.len());
    if files.is_empty() {
        eprintln!("bench-baselines: no files found — bench will produce zeros");
        std::process::exit(2);
    }

    // Variant inputs were picked to span common, less-common, and
    // rare-but-real symbol names so the reported [low, high] band
    // captures realistic variance rather than only the easy case.
    // For symbol-shaped tools (find_refs / find_symbol / callers /
    // callees) the chosen names are real identifiers in this repo.
    let common_symbols = &["new", "from", "default", "Result", "main"];
    let mid_symbols = &["validate", "Telemetry", "BASELINES", "code_outline"];
    let path_chain = &[
        "main",
        "ReconServer",
        "instrumented",
        "record_call",
        "Telemetry",
    ];
    let impact_symbols = &["validate", "code_outline"];
    let symbol_inputs: Vec<&'static str> = common_symbols
        .iter()
        .chain(mid_symbols.iter())
        .copied()
        .collect();
    let symbol_inputs_static: &[&'static str] = Box::leak(symbol_inputs.into_boxed_slice());

    eprintln!("bench-baselines: running alternative-loop simulators…\n");

    let measurements = vec![
        run_variants("code_find_refs", symbol_inputs_static, |s| {
            alternative_find_refs(&files, s)
        }),
        run_variants("code_find_symbol", symbol_inputs_static, |s| {
            alternative_find_symbol(&files, s)
        }),
        run_on_subsets("code_repo_map", &files, alternative_repo_map),
        run_variants("code_callers", symbol_inputs_static, |s| {
            alternative_callers_or_callees(&files, s)
        }),
        run_variants("code_callees", symbol_inputs_static, |s| {
            alternative_callers_or_callees(&files, s)
        }),
        run_on_subsets("code_path", &files, |subset| {
            alternative_path(subset, path_chain)
        }),
        run_variants("code_impact", impact_symbols, |s| {
            alternative_impact(&files, s)
        }),
        run_on_subsets("code_subsystems", &files, alternative_subsystems),
        run_on_subsets("code_subsystem", &files, alternative_subsystem),
    ];

    // Human-readable summary first so the operator can sanity-check
    // before consuming the Rust source snippet below.
    eprintln!(
        "{:<22} {:>10} {:>10} {:>10} {:>10}",
        "tool", "low", "median", "high", "avg_ms"
    );
    eprintln!("{}", "-".repeat(64));
    for m in &measurements {
        eprintln!(
            "{:<22} {:>10} {:>10} {:>10} {:>10}",
            m.tool,
            m.low(),
            m.median(),
            m.high(),
            m.avg_latency_ms(),
        );
    }
    eprintln!();

    // Drop-in Rust snippet on stdout so it can be captured cleanly:
    //   bench-baselines > /tmp/derived.txt
    println!("// Generated by `cargo run --release -p recon-cli --bin bench-baselines`.");
    println!("// Pasted ranges replace the asserted point estimates in BASELINES.");
    println!("// Migrated tools (code_outline, code_skeleton, code_read_symbol,");
    println!("// code_context, plus the index-driven static rows) are NOT in this list —");
    println!("// they keep their existing rows.");
    println!();
    for m in &measurements {
        print_baseline_literal(m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic micro-fixture so the unit tests don't depend on any
    /// real file layout. Three small "Rust files" with overlapping
    /// symbols give every simulator something to grep / read.
    fn fixture() -> (tempfile::TempDir, Vec<PathBuf>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let files = [
            ("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }\npub fn validate(x: u32) -> bool { x > 0 }\n"),
            ("src/main.rs", "fn main() { let _ = lib::add(1, 2); validate(3); }\n"),
            ("src/util.rs", "pub fn validate_email(s: &str) -> bool { s.contains('@') }\n"),
        ];
        let mut paths = Vec::new();
        for (rel, body) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            paths.push(path);
        }
        (tmp, paths)
    }

    #[test]
    fn word_boundary_matches_identifier() {
        assert!(matches_word_boundary("fn validate() {}", "validate"));
        assert!(matches_word_boundary("call validate() then", "validate"));
        // Substring without word boundary must NOT match: a real
        // grep `\bvalidate\b` won't pick `validate_email` for the
        // shorter needle.
        assert!(!matches_word_boundary("validate_email(x);", "validate"));
        assert!(!matches_word_boundary("invalidated", "validate"));
    }

    #[test]
    fn measurement_low_median_high_are_monotone() {
        let mut m = Measurement::new("test");
        m.record_run(100, 1_000);
        m.record_run(200, 1_000);
        m.record_run(50, 1_000);
        assert_eq!(m.low(), 50);
        assert_eq!(m.high(), 200);
        // median = middle of sorted [50, 100, 200]
        assert_eq!(m.median(), 100);
        assert!(m.low() <= m.median() && m.median() <= m.high());
    }

    #[test]
    fn alternative_find_refs_returns_nonzero_for_known_symbol() {
        let (_tmp, files) = fixture();
        let tokens = alternative_find_refs(&files, "validate");
        assert!(tokens > 0, "find_refs against a known symbol must return >0");
    }

    #[test]
    fn alternative_repo_map_includes_envelope_and_reads() {
        let (_tmp, files) = fixture();
        let tokens = alternative_repo_map(&files);
        assert!(
            tokens > 10,
            "repo_map of a 3-file fixture must produce a meaningful count"
        );
    }

    #[test]
    fn run_on_subsets_produces_three_data_points() {
        let (_tmp, files) = fixture();
        let m = run_on_subsets("test", &files, alternative_repo_map);
        // 3 cuts: 1/3, 2/3, full → 3 samples (or fewer if any cut
        // collapsed to 0, which shouldn't happen for a 3-file fixture).
        assert!(
            m.samples.len() >= 2 && m.samples.len() <= 3,
            "expected 2–3 samples from subset run, got {}",
            m.samples.len()
        );
        assert!(m.low() <= m.median() && m.median() <= m.high());
    }

    /// Regression guard: every alternative simulator must produce a
    /// non-zero, monotone (low ≤ median ≤ high) outcome on a fixture
    /// that contains the symbol it's searching for. Catches bugs
    /// like "filter accidentally drops every file" or "off-by-one in
    /// the cut math zeroed the band."
    #[test]
    fn every_simulator_produces_sensible_output() {
        let (_tmp, files) = fixture();
        let symbol = "validate";

        let m_refs = run_variants("code_find_refs", &["validate"], |s| {
            alternative_find_refs(&files, s)
        });
        assert!(m_refs.median() > 0);

        let m_sym = run_variants("code_find_symbol", &["validate"], |s| {
            alternative_find_symbol(&files, s)
        });
        assert!(m_sym.median() > 0);

        let m_repo = run_on_subsets("code_repo_map", &files, alternative_repo_map);
        assert!(m_repo.median() > 0);
        assert!(m_repo.low() <= m_repo.median() && m_repo.median() <= m_repo.high());

        let m_callers = run_variants("code_callers", &[symbol], |s| {
            alternative_callers_or_callees(&files, s)
        });
        assert!(m_callers.median() > 0);

        let m_path = run_on_subsets("code_path", &files, |subset| {
            alternative_path(subset, &["validate", "add"])
        });
        assert!(m_path.median() > 0);

        let m_impact = run_variants("code_impact", &[symbol], |s| {
            alternative_impact(&files, s)
        });
        assert!(m_impact.median() > 0);

        let m_subs = run_on_subsets("code_subsystems", &files, alternative_subsystems);
        assert!(m_subs.median() > 0);

        let m_sub = run_on_subsets("code_subsystem", &files, alternative_subsystem);
        assert!(m_sub.median() > 0);
    }
}
