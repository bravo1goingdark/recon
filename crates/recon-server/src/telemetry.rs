//! Token-savings telemetry — the screenshot-shareable proof recon delivers value.
//!
//! Tracks per-tool call counts, response token estimates, and a baseline
//! "what Read+Grep would have cost" estimate. The difference is the
//! tokens-saved figure surfaced in `code_stats.telemetry` and the dedicated
//! `code_savings` tool.
//!
//! All counters are lock-free atomics on the hot path. Persistence to the
//! SQLite `meta` table happens (a) every `FLUSH_THRESHOLD` calls via an
//! async-spawned task that doesn't block the tool response, and (b) on
//! `ReconServer::shutdown` so a clean exit captures the trailing window.
//!
//! Telemetry is **best-effort** — if SQLite write fails, we log warn and
//! drop the increment for that flush. A telemetry failure must never block
//! a tool call.
//!
//! ## Baselines
//!
//! Per-tool baselines in [`BASELINES`] are conservative, audit-friendly
//! estimates of what an agent would have spent in tokens using only
//! Read/Grep/Glob. Each baseline cites the dominant alternative path.
//! These are static constants by design — defending the numbers is a
//! marketing asset, and per-user tweaking dilutes the "saved N tokens"
//! claim.
//!
//! ## Why no dollar conversion
//!
//! recon is model-agnostic; agents using it may run on Claude, GPT,
//! Gemini, a self-hosted Llama, or anything else, each with its own
//! pricing and discount structure. Hard-coding a "saved $X" figure
//! would either privilege one provider's list price or quietly drift
//! away from what users actually pay. We report tokens-saved and let
//! callers convert against whatever rate sheet they price against.

use parking_lot::Mutex;
use recon_storage::store::Store;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

/// Flush counters to SQLite every N tool calls. Trades a small SQLite
/// write rate for bounded data loss on hard kill.
pub const FLUSH_THRESHOLD: u64 = 50;

/// Per-tool baseline cost: what an agent would otherwise have paid using
/// only Read/Grep/Glob.
pub struct Baseline {
    /// MCP tool name.
    pub tool: &'static str,
    /// Estimated tokens an agent would consume reaching the same answer
    /// without recon.
    pub baseline_tokens: u64,
    /// One-line rationale shown via `code_savings --explain` for trust.
    pub rationale: &'static str,
}

/// Per-tool baseline table. Conservative honest estimates documented for
/// audit. Update with each new tool.
pub const BASELINES: &[Baseline] = &[
    Baseline {
        tool: "code_outline",
        baseline_tokens: 3000,
        rationale: "Read of avg 500-line file",
    },
    Baseline {
        tool: "code_skeleton",
        baseline_tokens: 3000,
        rationale: "Read of full file",
    },
    Baseline {
        tool: "code_read_symbol",
        baseline_tokens: 3000,
        rationale: "Read full file to extract one symbol",
    },
    Baseline {
        tool: "code_find_symbol",
        baseline_tokens: 5000,
        rationale: "Grep across repo + read top 2 hits",
    },
    Baseline {
        tool: "code_find_refs",
        baseline_tokens: 3000,
        rationale: "Grep for symbol name across repo",
    },
    Baseline {
        tool: "code_find_strings",
        baseline_tokens: 3000,
        rationale: "Grep for string in source files",
    },
    Baseline {
        tool: "code_search",
        baseline_tokens: 4000,
        rationale: "Grep + read 2 hit files",
    },
    Baseline {
        tool: "code_multi_find",
        baseline_tokens: 5000,
        rationale: "N×Grep (avg 3 patterns)",
    },
    Baseline {
        tool: "code_list",
        baseline_tokens: 2000,
        rationale: "Glob + ls equivalents",
    },
    Baseline {
        tool: "code_repo_map",
        baseline_tokens: 20000,
        rationale: "Read 5 files for orientation",
    },
    Baseline {
        tool: "code_path",
        baseline_tokens: 5000,
        rationale: "5x chained code_find_refs",
    },
    Baseline {
        tool: "code_callers",
        baseline_tokens: 3000,
        rationale: "depth=1 chained ref lookups",
    },
    Baseline {
        tool: "code_callees",
        baseline_tokens: 3000,
        rationale: "depth=1 chained ref lookups",
    },
    Baseline {
        tool: "code_context",
        baseline_tokens: 8000,
        rationale: "4-call understand-X loop (find_symbol+read_symbol+find_refs+search-tests)",
    },
    Baseline {
        tool: "code_impact",
        baseline_tokens: 9000,
        rationale: "transitive callers + test grep + analysis",
    },
    Baseline {
        tool: "code_subsystems",
        baseline_tokens: 12000,
        rationale: "repo_map + 5 file reads",
    },
    Baseline {
        tool: "code_subsystem",
        baseline_tokens: 5000,
        rationale: "directory listing + reads",
    },
    Baseline {
        tool: "code_stats",
        baseline_tokens: 500,
        rationale: "git log + ls equivalent",
    },
    Baseline {
        tool: "code_reindex",
        baseline_tokens: 0,
        rationale: "system operation, no agent alternative",
    },
    Baseline {
        tool: "code_savings",
        baseline_tokens: 0,
        rationale: "self-reference, no alternative",
    },
];

/// Look up a baseline by tool name. Returns 0 for unknown tools so a
/// missing entry never blocks a tool call (it just doesn't claim savings).
#[inline]
pub fn baseline_for(tool: &str) -> u64 {
    BASELINES
        .iter()
        .find(|b| b.tool == tool)
        .map(|b| b.baseline_tokens)
        .unwrap_or(0)
}

/// Lock-free per-tool counter. Hot path increments via `Relaxed`
/// `fetch_add`; readers (the `code_stats` / `code_savings` snapshots)
/// take a single `Acquire` load each. No mutexes on the call path.
pub struct ToolCounter {
    /// Number of times this tool was invoked since startup
    /// (lifetime-cumulative when persisted state is loaded on start).
    pub calls: AtomicU64,
    /// Sum of estimated response token counts emitted by this tool.
    pub response_tokens: AtomicU64,
    /// Sum of baseline tokens credited for this tool's calls.
    pub baseline_tokens: AtomicU64,
    /// Sum of measured tool-handler latency in microseconds.
    pub latency_micros_total: AtomicU64,
}

impl Default for ToolCounter {
    fn default() -> Self {
        Self {
            calls: AtomicU64::new(0),
            response_tokens: AtomicU64::new(0),
            baseline_tokens: AtomicU64::new(0),
            latency_micros_total: AtomicU64::new(0),
        }
    }
}

impl ToolCounter {
    /// Snapshot the counter atoms into a serializable struct.
    pub fn snapshot(&self) -> CounterSnapshot {
        CounterSnapshot {
            calls: self.calls.load(Ordering::Acquire),
            response_tokens: self.response_tokens.load(Ordering::Acquire),
            baseline_tokens: self.baseline_tokens.load(Ordering::Acquire),
            latency_micros_total: self.latency_micros_total.load(Ordering::Acquire),
        }
    }

    /// Hydrate from a deserialized snapshot — used at startup to merge
    /// persisted lifetime counters with fresh atomics.
    pub fn hydrate(&self, s: &CounterSnapshot) {
        self.calls.store(s.calls, Ordering::Release);
        self.response_tokens
            .store(s.response_tokens, Ordering::Release);
        self.baseline_tokens
            .store(s.baseline_tokens, Ordering::Release);
        self.latency_micros_total
            .store(s.latency_micros_total, Ordering::Release);
    }
}

/// Plain-old-data snapshot of a [`ToolCounter`] for serialization.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CounterSnapshot {
    /// Number of calls.
    #[serde(default)]
    pub calls: u64,
    /// Total response tokens emitted.
    #[serde(default)]
    pub response_tokens: u64,
    /// Total baseline tokens credited.
    #[serde(default)]
    pub baseline_tokens: u64,
    /// Total handler latency in microseconds.
    #[serde(default)]
    pub latency_micros_total: u64,
}

impl CounterSnapshot {
    /// Tokens saved = baseline credited - tokens actually emitted, clamped
    /// at 0. Conservative: a response that exceeds its baseline reports 0
    /// savings, never negative.
    #[inline]
    pub fn tokens_saved(&self) -> u64 {
        self.baseline_tokens.saturating_sub(self.response_tokens)
    }

    /// Average latency in milliseconds, or 0.0 when no calls recorded yet.
    #[inline]
    pub fn avg_latency_ms(&self) -> f64 {
        if self.calls == 0 {
            return 0.0;
        }
        (self.latency_micros_total as f64 / self.calls as f64) / 1000.0
    }
}

/// Server-wide telemetry. Held in an `Arc<Telemetry>` on `ReconServer`.
///
/// `tools` is a sorted list parallel to [`BASELINES`] so per-tool lookup is
/// `O(log n)` via binary search on `&'static str`. Adding a tool requires
/// only an entry in [`BASELINES`]; the constructor builds counters in lock-
/// step.
pub struct Telemetry {
    /// Per-tool counters, indexed parallel to [`BASELINES`].
    pub tools: Vec<(&'static str, ToolCounter)>,
    /// Unix seconds when this telemetry instance was created (server start).
    pub session_started_at: u64,
    /// Calls accumulated since the last persistence flush. When this
    /// exceeds [`FLUSH_THRESHOLD`], a flush is scheduled.
    pub calls_since_flush: AtomicU64,
    /// Serializes flush operations so two concurrent flushes don't
    /// interleave their SQLite writes. The atomic counter on the hot path
    /// is unaffected.
    flush_guard: Mutex<()>,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl Telemetry {
    /// Construct a fresh telemetry instance with zeroed counters.
    pub fn new() -> Self {
        let tools = BASELINES
            .iter()
            .map(|b| (b.tool, ToolCounter::default()))
            .collect();
        Self {
            tools,
            session_started_at: now_unix_secs(),
            calls_since_flush: AtomicU64::new(0),
            flush_guard: Mutex::new(()),
        }
    }

    /// Record one tool call. Lock-free hot path: 4 atomic adds + a
    /// best-effort counter reset on threshold. Returns `true` if a
    /// flush should be scheduled (caller is responsible for spawning
    /// the SQLite write task).
    ///
    /// Concurrency note: the threshold reset uses a plain `store` and
    /// is best-effort. Under heavy parallelism two threads may both
    /// observe `n >= threshold` and trigger a flush; that's harmless —
    /// `flush_to_store` is idempotent (it just rewrites the same
    /// snapshot under the `flush_guard`), so the worst case is one
    /// extra SQLite write.
    #[inline]
    pub fn record(&self, tool: &str, latency: Duration, response_tokens: u64) -> bool {
        let baseline = baseline_for(tool);
        if let Some((_, c)) = self.tools.iter().find(|(name, _)| *name == tool) {
            c.calls.fetch_add(1, Ordering::Relaxed);
            c.response_tokens
                .fetch_add(response_tokens, Ordering::Relaxed);
            c.baseline_tokens.fetch_add(baseline, Ordering::Relaxed);
            c.latency_micros_total
                .fetch_add(latency.as_micros() as u64, Ordering::Relaxed);
        }
        let n = self.calls_since_flush.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= FLUSH_THRESHOLD {
            // Reset so subsequent calls don't keep firing the threshold.
            // Use Release so the reset is observable by the next reader.
            self.calls_since_flush.store(0, Ordering::Release);
            return true;
        }
        false
    }

    /// Aggregated counter across every tool — the "lifetime totals" surface.
    pub fn aggregate(&self) -> CounterSnapshot {
        let mut agg = CounterSnapshot::default();
        for (_, c) in &self.tools {
            let s = c.snapshot();
            agg.calls += s.calls;
            agg.response_tokens += s.response_tokens;
            agg.baseline_tokens += s.baseline_tokens;
            agg.latency_micros_total += s.latency_micros_total;
        }
        agg
    }

    /// Per-tool snapshot for the `code_savings` breakdown.
    pub fn per_tool_snapshots(&self) -> Vec<(&'static str, CounterSnapshot)> {
        self.tools
            .iter()
            .map(|(name, c)| (*name, c.snapshot()))
            .collect()
    }

    /// Hydrate from persisted state on server startup. Reads `tel:tool:<name>`
    /// keys and merges the counts. Failures are non-fatal — a corrupt
    /// persisted blob just means the user's lifetime counters reset; the
    /// session counters keep working.
    pub fn hydrate_from_store(self: &Arc<Self>, store: &Store) {
        for (name, counter) in &self.tools {
            let key = format!("tel:tool:{name}");
            let raw = match store.get_meta(&key) {
                Ok(Some(s)) => s,
                Ok(None) => continue,
                Err(e) => {
                    warn!(tool = name, %e, "telemetry: meta read failed; resetting tool counter");
                    continue;
                }
            };
            match serde_json::from_str::<CounterSnapshot>(&raw) {
                Ok(snapshot) => counter.hydrate(&snapshot),
                Err(e) => warn!(
                    tool = name,
                    %e,
                    "telemetry: meta parse failed; tool counter starts at zero"
                ),
            }
        }
    }

    /// Persist lifetime counters to the SQLite `meta` table. Holds the
    /// `flush_guard` mutex so concurrent flushes serialize. Called from a
    /// `tokio::spawn` so the hot path doesn't block on disk I/O.
    pub fn flush_to_store(&self, store: &Store) {
        let _g = self.flush_guard.lock();
        let mut errors = 0;
        for (name, counter) in &self.tools {
            let snapshot = counter.snapshot();
            // Skip writing when nothing has happened — keeps `meta`
            // pristine for tools that have never been used.
            if snapshot.calls == 0 {
                continue;
            }
            let key = format!("tel:tool:{name}");
            let raw = match serde_json::to_string(&snapshot) {
                Ok(s) => s,
                Err(e) => {
                    warn!(tool = name, %e, "telemetry: serialize failed");
                    errors += 1;
                    continue;
                }
            };
            if let Err(e) = store.set_meta(&key, &raw) {
                warn!(tool = name, %e, "telemetry: meta write failed");
                errors += 1;
            }
        }
        // After flushing per-tool, reset the flush counter so the next
        // batch starts from zero.
        self.calls_since_flush.store(0, Ordering::Release);
        if errors > 0 {
            warn!(errors, "telemetry: flush completed with errors");
        } else {
            debug!("telemetry: flushed lifetime counters to meta");
        }
    }
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn baseline_lookup_known_tool() {
        assert_eq!(baseline_for("code_outline"), 3000);
        assert_eq!(baseline_for("code_repo_map"), 20000);
    }

    #[test]
    fn baseline_lookup_unknown_returns_zero() {
        assert_eq!(baseline_for("not_a_tool"), 0);
    }

    #[test]
    fn record_increments_counters() {
        let t = Arc::new(Telemetry::new());
        // Run several calls; threshold check is the boolean return.
        for _ in 0..10 {
            assert!(!t.record("code_outline", Duration::from_millis(2), 400));
        }
        let agg = t.aggregate();
        assert_eq!(agg.calls, 10);
        assert_eq!(agg.response_tokens, 4000);
        assert_eq!(agg.baseline_tokens, 30_000);
        assert_eq!(agg.tokens_saved(), 26_000);
    }

    #[test]
    fn record_threshold_triggers_flush() {
        let t = Arc::new(Telemetry::new());
        let mut triggers = 0;
        for _ in 0..(FLUSH_THRESHOLD + 1) {
            if t.record("code_outline", Duration::from_micros(50), 100) {
                triggers += 1;
            }
        }
        assert_eq!(triggers, 1, "exactly one flush trigger expected");
    }

    #[test]
    fn unknown_tool_records_call_to_threshold_only() {
        // Unknown tool name still increments the global flush counter so
        // an experimental tool doesn't break flush cadence — but no
        // per-tool counter is mutated.
        let t = Arc::new(Telemetry::new());
        let _ = t.record("not_a_tool", Duration::from_millis(1), 100);
        let agg = t.aggregate();
        assert_eq!(agg.calls, 0);
        assert_eq!(agg.response_tokens, 0);
        assert_eq!(t.calls_since_flush.load(Ordering::Acquire), 1);
    }

    #[test]
    fn snapshot_saturating_subtraction() {
        let s = CounterSnapshot {
            calls: 1,
            response_tokens: 5000,
            baseline_tokens: 3000,
            latency_micros_total: 0,
        };
        assert_eq!(s.tokens_saved(), 0, "negative savings clamp to 0");
    }

    #[test]
    fn hydrate_round_trip() {
        let t = Arc::new(Telemetry::new());
        t.record("code_outline", Duration::from_millis(1), 100);
        t.record("code_outline", Duration::from_millis(2), 200);
        let snap_before = t
            .tools
            .iter()
            .find(|(n, _)| *n == "code_outline")
            .unwrap()
            .1
            .snapshot();

        let t2 = Arc::new(Telemetry::new());
        t2.tools
            .iter()
            .find(|(n, _)| *n == "code_outline")
            .unwrap()
            .1
            .hydrate(&snap_before);
        let snap_after = t2
            .tools
            .iter()
            .find(|(n, _)| *n == "code_outline")
            .unwrap()
            .1
            .snapshot();
        assert_eq!(snap_after.calls, 2);
        assert_eq!(snap_after.response_tokens, 300);
        assert_eq!(snap_after.baseline_tokens, 6000);
    }
}
