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
//! Two parallel sources of "what would Read+grep have cost":
//!
//! - **Migrated handlers** (the bucket-1 tools that have a clean Read /
//!   grep alternative path) compute the figure per-call from real bytes
//!   and pass it via `Telemetry::record(.., Some(measured))`. Their
//!   credit accumulates into `measured_baseline_tokens`.
//! - **Non-migrated handlers** (composite tools, `code_repo_map`,
//!   `code_stats`, etc.) pass `None` and accrue a static [`BASELINES`]
//!   entry into `static_baseline_tokens`. Each `BASELINES` row is a
//!   conservative, audit-friendly estimate documented inline.
//!
//! Exactly one of the two baseline counters increments per call. The
//! dashboard sums them; tooltip / disclaimer makes the source explicit.
//!
//! ## Why no dollar conversion
//!
//! recon is model-agnostic; agents using it may run on Claude, GPT,
//! Gemini, a self-hosted Llama, or anything else, each with its own
//! pricing and discount structure. Hard-coding a "saved $X" figure
//! would either privilege one provider's list price or quietly drift
//! away from what users actually pay. We report tokens-saved and let
//! callers convert against whatever rate sheet they price against.

use ahash::AHashMap;
use compact_str::CompactString;
use dashmap::DashMap;
use parking_lot::Mutex;
use recon_storage::store::Store;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

/// Flush counters to SQLite every N tool calls. Trades a small SQLite
/// write rate for bounded data loss on hard kill. Lowered from 50 to
/// 10 in v0.5.0 — the count-only trigger left bursty agentic flows
/// (3 calls/hr in an idle IDE window) with persisted state perpetually
/// stale; pair the lower count with the timer below so the tail of
/// any session also flushes.
pub const FLUSH_THRESHOLD: u64 = 10;

/// Periodic flush interval in seconds. Even idle sessions persist
/// counters at least once per [`FLUSH_INTERVAL_SECS`] so
/// `recon savings show` never reports "no telemetry" after an
/// active session. Override via `RECON_TELEMETRY_FLUSH_SECS` (0
/// disables the timer entirely; the count trigger still fires).
pub const FLUSH_INTERVAL_SECS: u64 = 60;

/// Per-tool baseline cost: what an agent would otherwise have paid using
/// only Read/Grep/Glob.
pub struct Baseline {
    /// MCP tool name.
    pub tool: &'static str,
    /// Estimated tokens an agent would consume reaching the same answer
    /// without recon.
    pub baseline_tokens: u64,
    /// Estimated wall-time the alternative Read/Grep loop would have
    /// taken per call, in milliseconds. The session receipt + dashboard
    /// surface `(baseline_latency_ms × calls) - actual_latency_micros`
    /// as "time saved" — the gut-feel number that lands harder than
    /// "saved 47 K tokens." Order-of-magnitude estimates are fine; same
    /// rationale as `baseline_tokens`.
    pub baseline_latency_ms: u64,
    /// One-line rationale shown via `code_savings --explain` for trust.
    pub rationale: &'static str,
}

/// Per-tool baseline table. Conservative honest estimates for tools
/// that have not been migrated to the per-call measured path. Migrated
/// handlers keep an entry here with `baseline_tokens: 0` so
/// [`Telemetry::new`] still allocates a [`ToolCounter`] for them, but
/// `baseline_for(tool)` returns 0 — the static counter never accrues.
/// Update with each new tool; flip an entry's `baseline_tokens` to 0
/// when the handler graduates to the measured path.
pub const BASELINES: &[Baseline] = &[
    // ── Migrated to per-call measurement (zeroed) ──────────────────
    Baseline {
        tool: "code_outline",
        baseline_tokens: 0,
        baseline_latency_ms: 200,
        rationale: "measured per-call against the indexed file",
    },
    Baseline {
        tool: "code_skeleton",
        baseline_tokens: 0,
        baseline_latency_ms: 200,
        rationale: "measured per-call against the indexed file",
    },
    Baseline {
        tool: "code_read_symbol",
        baseline_tokens: 0,
        baseline_latency_ms: 250,
        rationale: "measured per-call (full-file Read equivalent)",
    },
    // ── Static estimates (composite tools, no clean alternative) ───
    // Static-only: 3-tier exact/BM25/fuzzy search has no clean grep
    // alternative — fuzzy and BM25 ranking can't be reproduced by
    // a single grep pass, so a measured baseline would understate
    // the real Read+grep+read-top-N cost.
    Baseline {
        tool: "code_find_symbol",
        baseline_tokens: 5000,
        baseline_latency_ms: 800,
        rationale: "Grep across repo + read top 2 hits",
    },
    // Static-only: handler is index-driven (refs table, no grep on
    // the hot path). Computing a measured baseline would require an
    // extra grep pass per call just to size the alternative —
    // doubles the work without changing the answer. Keep static.
    Baseline {
        tool: "code_find_refs",
        baseline_tokens: 3000,
        baseline_latency_ms: 600,
        rationale: "Grep for symbol name across repo",
    },
    Baseline {
        tool: "code_find_strings",
        baseline_tokens: 0,
        baseline_latency_ms: 400,
        rationale: "measured per-call (sum of grep match-line tokens)",
    },
    Baseline {
        tool: "code_search",
        baseline_tokens: 0,
        baseline_latency_ms: 500,
        rationale: "measured per-call when grep path is taken; 0 for tantivy/semantic",
    },
    Baseline {
        tool: "code_multi_find",
        baseline_tokens: 0,
        baseline_latency_ms: 1000,
        rationale: "measured per-call (sum across all patterns + matches)",
    },
    Baseline {
        tool: "code_list",
        baseline_tokens: 0,
        baseline_latency_ms: 2000,
        rationale: "measured per-call (sum of path + lang label bytes)",
    },
    Baseline {
        tool: "code_repo_map",
        baseline_tokens: 20000,
        baseline_latency_ms: 5000,
        rationale: "Read 5 files for orientation",
    },
    Baseline {
        tool: "code_path",
        baseline_tokens: 5000,
        baseline_latency_ms: 2000,
        rationale: "5x chained code_find_refs",
    },
    Baseline {
        tool: "code_callers",
        baseline_tokens: 3000,
        baseline_latency_ms: 800,
        rationale: "depth=1 chained ref lookups",
    },
    Baseline {
        tool: "code_callees",
        baseline_tokens: 3000,
        baseline_latency_ms: 800,
        rationale: "depth=1 chained ref lookups",
    },
    Baseline {
        tool: "code_context",
        baseline_tokens: 0,
        baseline_latency_ms: 1500,
        rationale: "measured per-call (target file read; floor on the 4-call alternative)",
    },
    // Static-only: pure graph traversal (transitive callers + test
    // detector). No file I/O on the hot path, so the only honest
    // measured baseline would require running the alternative
    // grep-of-callers passes per call — that doubles work without
    // changing the answer. Static stays.
    Baseline {
        tool: "code_impact",
        baseline_tokens: 9000,
        baseline_latency_ms: 3000,
        rationale: "transitive callers + test grep + analysis",
    },
    // Static-only: pure connected-components computation over the
    // cached graph. No file I/O, no grep — the alternative cost
    // (orientation = repo_map + 5 file reads) is real but unmeasurable
    // from this handler without doing exactly that extra work.
    Baseline {
        tool: "code_subsystems",
        baseline_tokens: 12000,
        baseline_latency_ms: 4000,
        rationale: "repo_map + 5 file reads",
    },
    // Static-only: index-only lookup over a cluster's symbol metadata.
    // Same reasoning as `code_subsystems` — measuring the alternative
    // (`ls + cat top-N files`) requires the extra reads.
    Baseline {
        tool: "code_subsystem",
        baseline_tokens: 5000,
        baseline_latency_ms: 1500,
        rationale: "directory listing + reads",
    },
    // Operator/system tools — not exposed via MCP as of v0.4. They
    // still get [`ToolCounter`] entries so CLI invocations
    // (`recon stats`, `recon savings show`) can record latency / call
    // counts, but their baseline credit is zero since users — not
    // agents — invoke them.
    Baseline {
        tool: "code_stats",
        baseline_tokens: 0,
        baseline_latency_ms: 0,
        rationale: "CLI/operator tool, not agent-facing",
    },
    Baseline {
        tool: "code_reindex",
        baseline_tokens: 0,
        baseline_latency_ms: 0,
        rationale: "system operation, no agent alternative",
    },
    Baseline {
        tool: "code_savings",
        baseline_tokens: 0,
        baseline_latency_ms: 0,
        rationale: "CLI/operator tool, not agent-facing",
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

/// Look up the per-call latency baseline (ms) for a tool. Returns 0 for
/// unknown tools and for operator-only tools whose baseline is genuinely
/// "no agent alternative."
#[inline]
pub fn baseline_latency_ms_for(tool: &str) -> u64 {
    BASELINES
        .iter()
        .find(|b| b.tool == tool)
        .map(|b| b.baseline_latency_ms)
        .unwrap_or(0)
}

/// Lock-free per-tool counter. Hot path increments via `Relaxed`
/// `fetch_add`; readers (the `code_stats` / `code_savings` snapshots)
/// take a single `Acquire` load each. No mutexes on the call path.
///
/// Each call increments exactly one of `static_baseline_tokens` or
/// `measured_baseline_tokens`, never both. Migrated handlers (the
/// bucket-1 tools that ran a real Read / grep alternative) supply
/// `Some(measured)` to [`Telemetry::record`]; non-migrated handlers
/// pass `None` and accrue the static [`BASELINES`] entry instead.
pub struct ToolCounter {
    /// Number of times this tool was invoked since startup
    /// (lifetime-cumulative when persisted state is loaded on start).
    pub calls: AtomicU64,
    /// Sum of estimated response token counts emitted by this tool.
    pub response_tokens: AtomicU64,
    /// Sum of static [`BASELINES`] tokens for calls that did NOT supply
    /// a per-call measurement. Zero for migrated tools whose entries
    /// have been removed from `BASELINES`.
    pub static_baseline_tokens: AtomicU64,
    /// Sum of *measured* Read / grep alternative tokens for calls that
    /// supplied a per-call measurement. Zero for non-migrated tools.
    pub measured_baseline_tokens: AtomicU64,
    /// Sum of measured tool-handler latency in microseconds.
    pub latency_micros_total: AtomicU64,
}

impl Default for ToolCounter {
    fn default() -> Self {
        Self {
            calls: AtomicU64::new(0),
            response_tokens: AtomicU64::new(0),
            static_baseline_tokens: AtomicU64::new(0),
            measured_baseline_tokens: AtomicU64::new(0),
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
            static_baseline_tokens: self.static_baseline_tokens.load(Ordering::Acquire),
            measured_baseline_tokens: self.measured_baseline_tokens.load(Ordering::Acquire),
            latency_micros_total: self.latency_micros_total.load(Ordering::Acquire),
        }
    }

    /// Hydrate from a deserialized snapshot — used at startup to merge
    /// persisted lifetime counters with fresh atomics.
    pub fn hydrate(&self, s: &CounterSnapshot) {
        self.calls.store(s.calls, Ordering::Release);
        self.response_tokens
            .store(s.response_tokens, Ordering::Release);
        self.static_baseline_tokens
            .store(s.static_baseline_tokens, Ordering::Release);
        self.measured_baseline_tokens
            .store(s.measured_baseline_tokens, Ordering::Release);
        self.latency_micros_total
            .store(s.latency_micros_total, Ordering::Release);
    }
}

/// Plain-old-data snapshot of a [`ToolCounter`] for serialization.
///
/// `static_baseline_tokens` and `measured_baseline_tokens` are tagged
/// `#[serde(default)]` so snapshots persisted by older recon versions
/// (which only had `calls`, `response_tokens`, and `latency_micros_total`)
/// still deserialize cleanly on upgrade — missing fields hydrate as 0
/// rather than wiping the user's lifetime "tokens saved" counters.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CounterSnapshot {
    /// Number of calls.
    pub calls: u64,
    /// Total response tokens emitted.
    pub response_tokens: u64,
    /// Total static-baseline tokens credited (non-migrated tools).
    #[serde(default)]
    pub static_baseline_tokens: u64,
    /// Total measured-baseline tokens credited (migrated tools).
    #[serde(default)]
    pub measured_baseline_tokens: u64,
    /// Total handler latency in microseconds.
    pub latency_micros_total: u64,
}

impl CounterSnapshot {
    /// Tokens saved = (static + measured baselines) - response tokens,
    /// clamped at 0. The two baseline fields are mutually exclusive
    /// per-call — measured for migrated tools, static for the rest —
    /// so summing them gives the total credited baseline across all
    /// calls in the snapshot.
    #[inline]
    pub fn tokens_saved(&self) -> u64 {
        self.static_baseline_tokens
            .saturating_add(self.measured_baseline_tokens)
            .saturating_sub(self.response_tokens)
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
/// `tools` is an `AHashMap` keyed by tool name so per-tool lookup is `O(1)`
/// on the hot path (`record`). Adding a tool requires only an entry in
/// [`BASELINES`]; the constructor builds counters in lock-step.
pub struct Telemetry {
    /// Per-tool counters keyed by tool name. Entries are inserted exactly
    /// once during construction from [`BASELINES`] and never mutated again
    /// — only the contained atomics on each `ToolCounter` are updated, so
    /// concurrent reads are safe without an outer lock.
    pub tools: AHashMap<&'static str, ToolCounter>,
    /// Unix seconds when this telemetry instance was created (server start).
    pub session_started_at: u64,
    /// Calls accumulated since the last persistence flush. When this
    /// exceeds [`FLUSH_THRESHOLD`], a flush is scheduled.
    pub calls_since_flush: AtomicU64,
    /// Serializes flush operations so two concurrent flushes don't
    /// interleave their SQLite writes. The atomic counter on the hot path
    /// is unaffected.
    flush_guard: Mutex<()>,
    /// Process-scoped baseline-credit dedupe (closes #25). The first call
    /// against a `(tool, dedup_key)` pair credits the full Read+Grep
    /// alternative cost; subsequent calls against the same pair credit 0.
    /// This kills the per-call double-counting that drove ~3.5×
    /// over-statement on hot sessions where the same file / query / symbol
    /// got hit repeatedly.
    ///
    /// Process-scoped is the v0.5.1 simplification: for stdio MCP transport
    /// (the dominant case) one `recon serve` process serves exactly one
    /// rmcp session, so process-scoped is identical to session-scoped. For
    /// Streamable HTTP a single process can multiplex many sessions; that
    /// case under-counts (later sessions see the dedupe set populated by
    /// earlier ones). v0.5.2 will plumb the rmcp `RequestContext` session
    /// id through tool handlers to fix the HTTP case; until then, agents
    /// querying via HTTP should restart `recon serve` between conversations
    /// for exact accounting.
    dedup: DashMap<(&'static str, CompactString), ()>,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl Telemetry {
    /// Construct a fresh telemetry instance with zeroed counters.
    pub fn new() -> Self {
        let mut tools = AHashMap::with_capacity(BASELINES.len());
        for b in BASELINES {
            tools.insert(b.tool, ToolCounter::default());
        }
        Self {
            tools,
            session_started_at: now_unix_secs(),
            calls_since_flush: AtomicU64::new(0),
            flush_guard: Mutex::new(()),
            dedup: DashMap::new(),
        }
    }

    /// Returns `true` if this is the first time this process has credited
    /// a baseline for `(tool, key)`, `false` thereafter. Caller credits
    /// the full baseline only on the first occurrence and zero baseline
    /// on every repeat — see #25 for the per-tool key table.
    ///
    /// Atomic via `DashMap::insert` returning `None` only on first write.
    /// No mutex; safe to call from concurrent tool handlers.
    pub fn should_credit_baseline(&self, tool: &'static str, key: &str) -> bool {
        self.dedup
            .insert((tool, CompactString::new(key)), ())
            .is_none()
    }

    /// Reset the dedupe set. Used by tests; not exposed to the hot path.
    /// Production resets happen implicitly via process exit (every
    /// `recon serve` start gets a fresh `Telemetry` and therefore a fresh
    /// dedupe map).
    #[cfg(test)]
    pub fn reset_dedup(&self) {
        self.dedup.clear();
    }

    /// Record one tool call. Lock-free hot path: 4 atomic adds + a
    /// best-effort counter reset on threshold. Returns `true` if a
    /// flush should be scheduled (caller is responsible for spawning
    /// the SQLite write task).
    ///
    /// `measured_baseline` is `Some(n)` when the handler computed a
    /// per-call Read / grep alternative; otherwise `None`. Exactly one
    /// of `static_baseline_tokens` or `measured_baseline_tokens`
    /// accrues per call — never both. Migrated tools always pass
    /// `Some(_)` (their `BASELINES` entry is removed so static lookup
    /// is 0); non-migrated tools always pass `None` and accrue the
    /// static figure.
    ///
    /// Concurrency note: the threshold reset uses a plain `store` and
    /// is best-effort. Under heavy parallelism two threads may both
    /// observe `n >= threshold` and trigger a flush; that's harmless —
    /// `flush_to_store` is idempotent (it just rewrites the same
    /// snapshot under the `flush_guard`), so the worst case is one
    /// extra SQLite write.
    #[inline]
    pub fn record(
        &self,
        tool: &str,
        latency: Duration,
        response_tokens: u64,
        measured_baseline: Option<u64>,
    ) -> bool {
        if let Some(c) = self.tools.get(tool) {
            c.calls.fetch_add(1, Ordering::Relaxed);
            c.response_tokens
                .fetch_add(response_tokens, Ordering::Relaxed);
            c.latency_micros_total
                .fetch_add(latency.as_micros() as u64, Ordering::Relaxed);
            match measured_baseline {
                Some(m) => {
                    c.measured_baseline_tokens.fetch_add(m, Ordering::Relaxed);
                }
                None => {
                    c.static_baseline_tokens
                        .fetch_add(baseline_for(tool), Ordering::Relaxed);
                }
            }
        }
        let n = self.calls_since_flush.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= FLUSH_THRESHOLD {
            self.calls_since_flush.store(0, Ordering::Release);
            return true;
        }
        false
    }

    /// Aggregated counter across every tool — the "lifetime totals" surface.
    pub fn aggregate(&self) -> CounterSnapshot {
        let mut agg = CounterSnapshot::default();
        for c in self.tools.values() {
            let s = c.snapshot();
            agg.calls += s.calls;
            agg.response_tokens += s.response_tokens;
            agg.static_baseline_tokens += s.static_baseline_tokens;
            agg.measured_baseline_tokens += s.measured_baseline_tokens;
            agg.latency_micros_total += s.latency_micros_total;
        }
        agg
    }

    /// Per-tool snapshot for the `code_savings` breakdown. Order is the
    /// declaration order from [`BASELINES`] so consumers see a stable
    /// listing (the underlying `AHashMap` is unordered).
    pub fn per_tool_snapshots(&self) -> Vec<(&'static str, CounterSnapshot)> {
        BASELINES
            .iter()
            .filter_map(|b| self.tools.get(b.tool).map(|c| (b.tool, c.snapshot())))
            .collect()
    }

    /// Aggregate "wall-time saved" across every tool call recorded so
    /// far, in microseconds. Computed as
    /// `Σ (baseline_latency_ms × calls × 1000) - Σ latency_micros_total`,
    /// clamped at 0 per tool so a slow recon path doesn't pull the
    /// total negative.
    ///
    /// Surfaced on the session receipt (#19) and pushed to the
    /// dashboard via `latency_saved_micros` on the `usage_rollups` row.
    pub fn latency_saved_micros(&self) -> u64 {
        let mut total: u64 = 0;
        for (tool, snap) in self.per_tool_snapshots() {
            let baseline_us = baseline_latency_ms_for(tool)
                .saturating_mul(snap.calls)
                .saturating_mul(1000);
            total = total.saturating_add(baseline_us.saturating_sub(snap.latency_micros_total));
        }
        total
    }

    /// Hydrate from persisted state on server startup. Reads `tel:tool:<name>`
    /// keys and merges the counts. Failures are non-fatal — a corrupt
    /// persisted blob just means the user's lifetime counters reset; the
    /// session counters keep working.
    pub fn hydrate_from_store(self: &Arc<Self>, store: &Store) {
        for (name, counter) in self.tools.iter() {
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
        for (name, counter) in self.tools.iter() {
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
        // Composite tools keep their static baselines.
        assert_eq!(baseline_for("code_repo_map"), 20000);
        assert_eq!(baseline_for("code_find_symbol"), 5000);
    }

    #[test]
    fn baseline_lookup_unknown_returns_zero() {
        assert_eq!(baseline_for("not_a_tool"), 0);
    }

    #[test]
    fn baseline_lookup_migrated_tool_returns_zero() {
        // Migrated handlers always supply a measured baseline; their
        // static lookup must be 0 so the static counter never accrues
        // for them.
        assert_eq!(baseline_for("code_outline"), 0);
        assert_eq!(baseline_for("code_skeleton"), 0);
        assert_eq!(baseline_for("code_read_symbol"), 0);
    }

    #[test]
    fn record_static_path_for_non_migrated_tool() {
        let t = Arc::new(Telemetry::new());
        // First 9 calls accumulate without firing the flush trigger.
        for _ in 0..(FLUSH_THRESHOLD - 1) {
            assert!(!t.record("code_find_symbol", Duration::from_millis(2), 400, None));
        }
        // 10th call hits the threshold and returns true.
        assert!(t.record("code_find_symbol", Duration::from_millis(2), 400, None));
        let agg = t.aggregate();
        assert_eq!(agg.calls, 10);
        assert_eq!(agg.response_tokens, 4000);
        // 10 × 5000 (code_find_symbol static).
        assert_eq!(agg.static_baseline_tokens, 50_000);
        assert_eq!(agg.measured_baseline_tokens, 0);
        assert_eq!(agg.tokens_saved(), 46_000);
    }

    #[test]
    fn record_measured_path_for_migrated_tool() {
        let t = Arc::new(Telemetry::new());
        t.record("code_outline", Duration::from_millis(1), 200, Some(2500));
        t.record("code_outline", Duration::from_millis(1), 250, Some(2700));
        let agg = t.aggregate();
        assert_eq!(agg.calls, 2);
        assert_eq!(agg.response_tokens, 450);
        // Migrated tool — static credit is zero, measured holds the truth.
        assert_eq!(agg.static_baseline_tokens, 0);
        assert_eq!(agg.measured_baseline_tokens, 5200);
        assert_eq!(agg.tokens_saved(), 4750);
    }

    #[test]
    fn record_threshold_triggers_flush() {
        let t = Arc::new(Telemetry::new());
        let mut triggers = 0;
        for _ in 0..(FLUSH_THRESHOLD + 1) {
            if t.record("code_find_symbol", Duration::from_micros(50), 100, None) {
                triggers += 1;
            }
        }
        assert_eq!(triggers, 1, "exactly one flush trigger expected");
    }

    /// First-sight of a (tool, key) pair returns true; every subsequent
    /// call against the same key returns false. A different key on the
    /// same tool counts as fresh. Closes #25 at the API surface.
    #[test]
    fn dedup_first_then_repeats_then_different_key() {
        let t = Telemetry::new();
        // First credit on (code_outline, "src/lib.rs") — true.
        assert!(t.should_credit_baseline("code_outline", "src/lib.rs"));
        // Repeat — false (don't credit baseline a second time).
        assert!(!t.should_credit_baseline("code_outline", "src/lib.rs"));
        assert!(!t.should_credit_baseline("code_outline", "src/lib.rs"));
        // Different file under the same tool — credit again.
        assert!(t.should_credit_baseline("code_outline", "src/main.rs"));
        // Same file under a different tool — credit (different alternative
        // would have happened: outline reads, find_symbol greps).
        assert!(t.should_credit_baseline("code_skeleton", "src/lib.rs"));
        // After reset, every key is fresh again.
        t.reset_dedup();
        assert!(t.should_credit_baseline("code_outline", "src/lib.rs"));
    }

    /// The dedup gate must not affect calls / response / latency counters —
    /// only baseline accrual is suppressed on repeats.
    #[test]
    fn dedup_only_gates_baseline_not_calls() {
        let t = Arc::new(Telemetry::new());
        // First call: full static baseline (5000 for code_find_symbol) accrues.
        assert!(t.should_credit_baseline("code_find_symbol", "Foo"));
        let _ = t.record("code_find_symbol", Duration::from_millis(2), 400, None);
        // Repeat: caller passes Some(0) to suppress baseline; call & response
        // tokens still accrue.
        assert!(!t.should_credit_baseline("code_find_symbol", "Foo"));
        let _ = t.record("code_find_symbol", Duration::from_millis(2), 400, Some(0));
        let agg = t.aggregate();
        assert_eq!(agg.calls, 2, "both calls counted");
        assert_eq!(agg.response_tokens, 800, "both response sums counted");
        // Only the first call accrued the static baseline.
        assert_eq!(agg.static_baseline_tokens, 5000);
        assert_eq!(agg.measured_baseline_tokens, 0);
    }

    #[test]
    fn unknown_tool_records_call_to_threshold_only() {
        // Unknown tool name still increments the global flush counter so
        // an experimental tool doesn't break flush cadence — but no
        // per-tool counter is mutated, regardless of whether a measured
        // baseline was supplied.
        let t = Arc::new(Telemetry::new());
        let _ = t.record("not_a_tool", Duration::from_millis(1), 100, Some(500));
        let agg = t.aggregate();
        assert_eq!(agg.calls, 0);
        assert_eq!(agg.response_tokens, 0);
        assert_eq!(agg.static_baseline_tokens, 0);
        assert_eq!(agg.measured_baseline_tokens, 0);
        assert_eq!(t.calls_since_flush.load(Ordering::Acquire), 1);
    }

    #[test]
    fn snapshot_saturating_subtraction() {
        let s = CounterSnapshot {
            calls: 2,
            response_tokens: 5000,
            static_baseline_tokens: 1000,
            measured_baseline_tokens: 2000,
            latency_micros_total: 0,
        };
        // (1000 + 2000) - 5000 saturates to 0.
        assert_eq!(s.tokens_saved(), 0, "negative savings clamp to 0");
    }

    #[test]
    fn tokens_saved_sums_both_baseline_sources() {
        let s = CounterSnapshot {
            calls: 5,
            response_tokens: 800,
            static_baseline_tokens: 6000,
            measured_baseline_tokens: 4000,
            latency_micros_total: 0,
        };
        assert_eq!(s.tokens_saved(), 9200);
    }

    #[test]
    fn hydrate_round_trip() {
        let t = Arc::new(Telemetry::new());
        t.record("code_outline", Duration::from_millis(1), 100, Some(2400));
        t.record("code_outline", Duration::from_millis(2), 200, Some(2800));
        let snap_before = t.tools.get("code_outline").unwrap().snapshot();

        let t2 = Arc::new(Telemetry::new());
        t2.tools.get("code_outline").unwrap().hydrate(&snap_before);
        let snap_after = t2.tools.get("code_outline").unwrap().snapshot();
        assert_eq!(snap_after.calls, 2);
        assert_eq!(snap_after.response_tokens, 300);
        assert_eq!(snap_after.static_baseline_tokens, 0);
        assert_eq!(snap_after.measured_baseline_tokens, 5200);
    }

    /// Snapshots persisted by older recon versions lacked the
    /// `static_baseline_tokens` and `measured_baseline_tokens` fields.
    /// They MUST still deserialize on upgrade — otherwise lifetime
    /// tokens-saved counters silently reset to zero on every startup.
    #[test]
    fn counter_snapshot_deserializes_old_blob_without_baseline_fields() {
        let legacy = r#"{"calls":42,"response_tokens":1234,"latency_micros_total":99999}"#;
        let snap: CounterSnapshot =
            serde_json::from_str(legacy).expect("legacy snapshot must deserialize");
        assert_eq!(snap.calls, 42);
        assert_eq!(snap.response_tokens, 1234);
        assert_eq!(snap.latency_micros_total, 99999);
        assert_eq!(snap.static_baseline_tokens, 0);
        assert_eq!(snap.measured_baseline_tokens, 0);
    }
}
