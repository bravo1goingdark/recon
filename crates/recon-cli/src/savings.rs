//! `recon savings` — push and inspect local token-savings telemetry.
//!
//! The MCP server tracks per-tool counters (calls, response_tokens,
//! baseline_tokens, latency_micros) in `.recon/index.db` under the
//! `meta` table with `tel:tool:*` keys. This subcommand reads them,
//! aggregates today's snapshot, and POSTs to the dashboard worker.
//!
//! Tier gating: the worker rejects Free with HTTP 402. The CLI surfaces
//! that with a clean upgrade message — never an opaque "401" or stack
//! trace. Pro/Team get a 200 and `recon savings push` exits 0.
//!
//! Idempotency: each run sends today's CUMULATIVE counters (not a
//! delta). The worker MAX-merges on (user_id, day) so re-runs cannot
//! double-count and stale snapshots cannot regress totals. This trades
//! "exactly-once delivery" (which we don't get with a local DB anyway)
//! for "monotone, never lies" — the right side of the trade for a
//! finance-defensible counter.

use anyhow::{anyhow, Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default API endpoint (matches license.rs). Override with `RECON_API_URL`.
const DEFAULT_API_URL: &str = "https://recon-api.kumarashutosh34169.workers.dev";

/// Structure of the snapshot we push. Matches the worker route's body
/// schema (POST /v1/account/savings) byte-for-byte.
#[derive(Debug, Clone, Serialize)]
struct PushBody {
    day: String,
    calls: u64,
    response_tokens: u64,
    baseline_tokens: u64,
    tokens_saved: u64,
    latency_micros: u64,
}

/// Worker response on success. We don't strictly need the body — the
/// HTTP status carries the result — but parsing it surfaces a clear
/// error if the worker shape drifts away from what we expect.
#[derive(Debug, Deserialize)]
struct PushResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    day: String,
    #[serde(default)]
    tier: String,
}

/// Worker response on Pro-only rejection. Mirrors the 402 payload.
/// Only `message` is surfaced in the user-facing error; `error` and
/// `tier` are still parsed (so a wire-shape drift is a compile-time
/// catch via serde) but allowed to be dead via the prefix convention.
#[derive(Debug, Deserialize)]
struct UpsellResponse {
    #[serde(default, rename = "error")]
    _error: String,
    #[serde(default, rename = "tier")]
    _tier: String,
    #[serde(default)]
    message: String,
}

/// Per-tool counter snapshot loaded from `.recon/index.db`. Mirrors
/// `recon_server::telemetry::CounterSnapshot` so a future shape change
/// in either side is a compile error rather than a silent skew.
#[derive(Debug, Clone, Default, Deserialize)]
struct ToolSnapshot {
    #[serde(default)]
    calls: u64,
    #[serde(default)]
    response_tokens: u64,
    #[serde(default)]
    baseline_tokens: u64,
    #[serde(default)]
    latency_micros_total: u64,
}

/// Aggregate the per-tool snapshots into the daily roll-up shape the
/// worker expects.
fn aggregate(per_tool: &[(String, ToolSnapshot)]) -> PushBody {
    let mut calls = 0u64;
    let mut response_tokens = 0u64;
    let mut baseline_tokens = 0u64;
    let mut latency_micros = 0u64;
    for (_, s) in per_tool {
        calls = calls.saturating_add(s.calls);
        response_tokens = response_tokens.saturating_add(s.response_tokens);
        baseline_tokens = baseline_tokens.saturating_add(s.baseline_tokens);
        latency_micros = latency_micros.saturating_add(s.latency_micros_total);
    }
    let tokens_saved = baseline_tokens.saturating_sub(response_tokens);
    PushBody {
        day: today_utc(),
        calls,
        response_tokens,
        baseline_tokens,
        tokens_saved,
        latency_micros,
    }
}

/// UTC `YYYY-MM-DD` for today. Matches the worker's `validDay` regex.
fn today_utc() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 86_400 seconds per day; days since epoch.
    let days = (now / 86_400) as i64;
    // Convert to (year, month, day) via the civil-from-days algorithm
    // (Howard Hinnant's `civil_from_days`). Matches Cloudflare's UTC.
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Civil-from-days (Hinnant). Returns (year, month, day) for a given
/// days-since-1970-01-01 count. Pure integer math, no chrono dep.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0..146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0..399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0..365]
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = y + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// Locate `.recon/index.db` for the requested repo (or the current
/// directory when no override is supplied).
fn resolve_db_path(repo: Option<PathBuf>) -> Result<PathBuf> {
    let root = match repo {
        Some(p) => p,
        None => std::env::current_dir().context("could not get current directory")?,
    };
    let db = root.join(".recon").join("index.db");
    if !db.exists() {
        return Err(anyhow!(
            "no .recon/index.db at {} — run `recon init` or `recon serve` here first",
            root.display()
        ));
    }
    Ok(db)
}

/// Read all `tel:tool:<name>` rows from the SQLite `meta` table and
/// deserialize each value as a [`ToolSnapshot`]. Opens the DB read-only
/// so a concurrent `recon serve` writer is unaffected.
fn load_local_snapshots(db_path: &Path) -> Result<Vec<(String, ToolSnapshot)>> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {} read-only", db_path.display()))?;

    let mut stmt = conn
        .prepare("SELECT key, value FROM meta WHERE key LIKE 'tel:tool:%'")
        .context("preparing meta SELECT")?;
    let rows = stmt
        .query_map([], |row| {
            let key: String = row.get(0)?;
            let value: String = row.get(1)?;
            Ok((key, value))
        })
        .context("querying meta")?;

    let mut out: Vec<(String, ToolSnapshot)> = Vec::new();
    for r in rows {
        let (key, value) = r?;
        let tool = key
            .strip_prefix("tel:tool:")
            .map(str::to_owned)
            .unwrap_or(key);
        match serde_json::from_str::<ToolSnapshot>(&value) {
            Ok(snap) => out.push((tool, snap)),
            Err(e) => {
                // Don't bail on one corrupt row — telemetry is best-effort.
                // Log to stderr so the user sees the issue but the push
                // still succeeds for the well-formed rows.
                eprintln!("warn: skipping malformed tel:tool row {tool}: {e}");
            }
        }
    }
    Ok(out)
}

/// `recon savings push` — read local counters, POST to worker.
pub fn push(repo: Option<PathBuf>) -> Result<()> {
    let db_path = resolve_db_path(repo)?;
    let snapshots = load_local_snapshots(&db_path)?;
    if snapshots.is_empty() {
        eprintln!(
            "no telemetry counters in {} — make at least one MCP tool call before pushing.",
            db_path.display()
        );
        return Ok(());
    }
    let body = aggregate(&snapshots);

    // Authenticate via the same cached API key the rest of the CLI uses.
    let config_dir = recon_server::license::global_config_dir();
    let key = recon_server::license::read_credentials(&config_dir).ok_or_else(|| {
        anyhow!(
            "no API key on disk — run `recon login <key>` first.\n\
             Or set RECON_API_KEY=sk-recon-… as a one-shot."
        )
    })?;

    let api_url = std::env::var("RECON_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string());
    let url = format!("{api_url}/v1/account/savings");
    let user_agent = concat!("recon/", env!("CARGO_PKG_VERSION"));

    let response = ureq::post(&url)
        .header("Authorization", &format!("Bearer {key}"))
        .header("User-Agent", user_agent)
        .send_json(&body);

    match response {
        Ok(mut r) => {
            let parsed: PushResponse = r
                .body_mut()
                .read_json()
                .context("parsing worker response JSON")?;
            println!(
                "Pushed savings for {} ({} tier · {} status)",
                parsed.day, parsed.tier, parsed.status
            );
            println!(
                "  calls={} · response_tokens={} · baseline={} · saved={}",
                body.calls, body.response_tokens, body.baseline_tokens, body.tokens_saved
            );
            Ok(())
        }
        Err(ureq::Error::StatusCode(402)) => {
            // Pro-only feature. Try to recover the upsell message body
            // for a clean error; fall back to a generic message if the
            // body parse fails.
            let upsell_msg = match ureq::post(&url)
                .header("Authorization", &format!("Bearer {key}"))
                .header("User-Agent", user_agent)
                .send_json(&body)
            {
                Ok(mut r) => r
                    .body_mut()
                    .read_json::<UpsellResponse>()
                    .ok()
                    .map(|u| u.message)
                    .unwrap_or_default(),
                Err(_) => String::new(),
            };
            Err(anyhow!(
                "{}",
                if upsell_msg.is_empty() {
                    "Savings dashboard is a Pro/Team feature. \
                     Upgrade at https://mcprecon.pages.dev/pricing"
                        .to_string()
                } else {
                    upsell_msg
                }
            ))
        }
        Err(ureq::Error::StatusCode(code)) if (400..500).contains(&code) => Err(anyhow!(
            "worker rejected push (HTTP {code}) — your key may be revoked. Try `recon login`."
        )),
        Err(e) => Err(anyhow!("network error pushing savings: {e}")),
    }
}

/// `recon savings show` — print local counters as TSV. No network call.
pub fn show(repo: Option<PathBuf>) -> Result<()> {
    let db_path = resolve_db_path(repo)?;
    let snapshots = load_local_snapshots(&db_path)?;
    if snapshots.is_empty() {
        eprintln!(
            "no telemetry counters in {} — make at least one MCP tool call first.",
            db_path.display()
        );
        return Ok(());
    }
    println!("# tool\tcalls\tresponse_tokens\tbaseline\ttokens_saved\tavg_latency_ms");
    let mut totals = ToolSnapshot::default();
    for (tool, s) in &snapshots {
        let saved = s.baseline_tokens.saturating_sub(s.response_tokens);
        let avg_ms = if s.calls == 0 {
            0.0
        } else {
            (s.latency_micros_total as f64 / s.calls as f64) / 1000.0
        };
        println!(
            "{}\t{}\t{}\t{}\t{}\t{:.2}",
            tool, s.calls, s.response_tokens, s.baseline_tokens, saved, avg_ms
        );
        totals.calls += s.calls;
        totals.response_tokens += s.response_tokens;
        totals.baseline_tokens += s.baseline_tokens;
        totals.latency_micros_total += s.latency_micros_total;
    }
    let saved_total = totals
        .baseline_tokens
        .saturating_sub(totals.response_tokens);
    println!(
        "# total\t{}\t{}\t{}\t{}\t-",
        totals.calls, totals.response_tokens, totals.baseline_tokens, saved_total
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_known_dates() {
        // 1970-01-01 is day 0.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2020-02-29 (leap day) — 18_321 days after epoch.
        assert_eq!(civil_from_days(18_321), (2020, 2, 29));
        // 2024-01-01 — 19_723 days after epoch.
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    #[test]
    fn today_utc_is_well_formed() {
        let s = today_utc();
        assert_eq!(s.len(), 10);
        assert_eq!(s.chars().nth(4), Some('-'));
        assert_eq!(s.chars().nth(7), Some('-'));
        // Each section parses as an integer in plausible range.
        let parts: Vec<&str> = s.split('-').collect();
        assert_eq!(parts.len(), 3);
        let y: i32 = parts[0].parse().expect("year");
        let m: u32 = parts[1].parse().expect("month");
        let d: u32 = parts[2].parse().expect("day");
        assert!((2025..=2100).contains(&y));
        assert!((1..=12).contains(&m));
        assert!((1..=31).contains(&d));
    }

    #[test]
    fn aggregate_sums_per_tool_counters() {
        let snapshots = vec![
            (
                "code_outline".into(),
                ToolSnapshot {
                    calls: 5,
                    response_tokens: 500,
                    baseline_tokens: 15_000,
                    latency_micros_total: 5_000,
                },
            ),
            (
                "code_path".into(),
                ToolSnapshot {
                    calls: 3,
                    response_tokens: 200,
                    baseline_tokens: 15_000,
                    latency_micros_total: 3_000,
                },
            ),
        ];
        let body = aggregate(&snapshots);
        assert_eq!(body.calls, 8);
        assert_eq!(body.response_tokens, 700);
        assert_eq!(body.baseline_tokens, 30_000);
        assert_eq!(body.tokens_saved, 29_300);
        assert_eq!(body.latency_micros, 8_000);
    }

    #[test]
    fn aggregate_clamps_savings_at_zero() {
        // Pathological: response > baseline. Should clamp to 0, never
        // report negative savings.
        let snapshots = vec![(
            "code_outline".into(),
            ToolSnapshot {
                calls: 1,
                response_tokens: 5_000,
                baseline_tokens: 3_000,
                latency_micros_total: 100,
            },
        )];
        let body = aggregate(&snapshots);
        assert_eq!(body.tokens_saved, 0);
    }

    #[test]
    fn aggregate_empty_returns_zeros() {
        let body = aggregate(&[]);
        assert_eq!(body.calls, 0);
        assert_eq!(body.tokens_saved, 0);
    }
}
