//! Calibration harness for the measured-baselines rollout.
//!
//! Builds a [`recon_server::server::ReconServer`] over the given repo,
//! invokes each migrated bucket-1 tool with a representative argument
//! payload, and reports per-tool `(static_estimate, measured, response,
//! divergence)` as TSV on stdout. Returns `Ok(true)` iff the divergence
//! gate passes — see [`PASS_THRESHOLD_RATIO`] / [`PASS_TOOL_COUNT`].
//!
//! The static estimates here are the historical pre-measurement
//! `BASELINES` values that the dashboard claim was previously built
//! on. The harness validates that the per-call measurement converges
//! to within 15% of those numbers on a real repo — if they diverge
//! more than that, either the static guess was wrong or the measured
//! path has a bug, and the v2 default-flip should not happen until
//! the gap is reconciled.

use std::path::Path;

use recon_search::tantivy_backend::TantivyBackend;
use recon_server::server::ReconServer;
use recon_server::telemetry::CounterSnapshot;
use recon_storage::store::Store;

/// Per-tool divergence gate. Set per the measured-savings plan: the
/// measured baseline must agree with the historical static estimate
/// to within 15% in absolute relative terms before the default-on
/// flip is safe.
const PASS_THRESHOLD_RATIO: f64 = 0.15;

/// Minimum number of tools (out of the calibrated set) that must pass
/// the divergence gate. Two outliers are tolerated to absorb noise on
/// tiny inputs / search patterns that don't have many matches.
const PASS_TOOL_COUNT: usize = 5;

/// The bucket-1 tools that ship measured per-call baselines, paired
/// with the historical static estimate the dashboard previously
/// reported and a representative argument payload to exercise the
/// handler against the calibration repo.
struct CalibrationCase {
    tool: &'static str,
    static_estimate: u64,
    args: serde_json::Value,
}

fn cases() -> Vec<CalibrationCase> {
    vec![
        CalibrationCase {
            tool: "code_outline",
            static_estimate: 3000,
            args: serde_json::json!({ "path": "Cargo.toml" }),
        },
        CalibrationCase {
            tool: "code_skeleton",
            static_estimate: 3000,
            args: serde_json::json!({ "path": "Cargo.toml" }),
        },
        CalibrationCase {
            tool: "code_search",
            static_estimate: 4000,
            // regex mode forces the grep path so the measured baseline
            // is non-zero on any repo; \w+ matches every identifier-ish
            // token, an over-conservative upper bound on what an
            // unbounded grep would have emitted.
            args: serde_json::json!({ "query": "fn ", "mode": "regex" }),
        },
        CalibrationCase {
            tool: "code_find_strings",
            static_estimate: 3000,
            args: serde_json::json!({ "pattern": "TODO", "kind": "both" }),
        },
        CalibrationCase {
            tool: "code_multi_find",
            static_estimate: 5000,
            args: serde_json::json!({ "patterns": ["fn ", "struct ", "impl "] }),
        },
        CalibrationCase {
            tool: "code_list",
            static_estimate: 2000,
            args: serde_json::json!({}),
        },
        CalibrationCase {
            tool: "code_context",
            static_estimate: 8000,
            // Use a symbol the calibration repo is guaranteed to expose.
            // `main` lives in basically every Rust binary; if a target
            // repo doesn't have it, the call returns NotFound and the
            // measured value stays 0 — same outcome as a too-restrictive
            // pattern, and the test report makes that visible.
            args: serde_json::json!({ "symbol": "main", "token_budget": 2000 }),
        },
    ]
}

/// Build a server over `repo`, run the calibration cases, print TSV,
/// return whether the run satisfies the pass gate.
pub async fn run(repo: &Path) -> Result<bool, String> {
    if !repo.exists() {
        return Err(format!("repo path does not exist: {}", repo.display()));
    }

    // Cohabit with any existing `.recon/` to avoid trampling the user's
    // active index — write to a sibling working directory the harness
    // owns and reindexes from scratch.
    let workdir = repo.join(".recon-calibration");
    let _ = std::fs::remove_dir_all(&workdir);
    std::fs::create_dir_all(&workdir).map_err(|e| format!("could not create workdir: {e}"))?;
    let store_path = workdir.join("recon.db");
    let tantivy_dir = workdir.join("tantivy");
    std::fs::create_dir_all(&tantivy_dir).map_err(|e| format!("tantivy dir: {e}"))?;
    let store = Store::open(&store_path).map_err(|e| format!("store: {e}"))?;
    let tantivy = TantivyBackend::open(&tantivy_dir).map_err(|e| format!("tantivy: {e}"))?;
    let server = ReconServer::new(repo.to_path_buf(), store, tantivy)
        .map_err(|e| format!("server new: {e}"))?;
    server
        .index_repo()
        .await
        .map_err(|e| format!("index_repo: {e}"))?;

    println!("tool\tstatic_estimate\tmeasured\tresponse\tdivergence_pct\tpass");

    let cases = cases();
    let mut passes = 0usize;
    let mut report: Vec<(String, bool)> = Vec::new();
    for case in &cases {
        // Each call goes through the public dispatch path, so what the
        // calibration measures is identical to what production agents
        // will see — no shortcuts, no test-only branches.
        let _ = server.query_tool(case.tool, &case.args.to_string()).await;
        let snapshot = snapshot_for(&server, case.tool);
        let measured = snapshot.measured_baseline_tokens;
        let response = snapshot.response_tokens;
        let divergence = if case.static_estimate == 0 {
            0.0
        } else {
            (measured as f64 - case.static_estimate as f64).abs() / case.static_estimate as f64
        };
        let pass = divergence < PASS_THRESHOLD_RATIO;
        if pass {
            passes += 1;
        }
        println!(
            "{}\t{}\t{}\t{}\t{:.1}\t{}",
            case.tool,
            case.static_estimate,
            measured,
            response,
            divergence * 100.0,
            if pass { "PASS" } else { "FAIL" }
        );
        report.push((case.tool.to_string(), pass));
    }

    let overall = passes >= PASS_TOOL_COUNT;
    eprintln!();
    eprintln!(
        "{} of {} tools within ±{:.0}% of static estimate (need ≥ {})",
        passes,
        cases.len(),
        PASS_THRESHOLD_RATIO * 100.0,
        PASS_TOOL_COUNT,
    );
    if overall {
        eprintln!("PASS");
    } else {
        eprintln!("FAIL");
        for (tool, pass) in &report {
            if !pass {
                eprintln!("  - {tool} diverged > {:.0}%", PASS_THRESHOLD_RATIO * 100.0);
            }
        }
    }

    let _ = std::fs::remove_dir_all(&workdir);
    Ok(overall)
}

fn snapshot_for(server: &ReconServer, name: &str) -> CounterSnapshot {
    server
        .telemetry_arc()
        .per_tool_snapshots()
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| s)
        .unwrap_or_default()
}
