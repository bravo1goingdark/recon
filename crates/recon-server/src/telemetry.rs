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

/// Meta key carrying the encoder version for persisted baseline /
/// response token counters. Bumped when the encoder changes meaning,
/// so `hydrate_from_store` can drop pre-bump counters whose units no
/// longer match what fresh atomics will accrue.
const ENCODER_VERSION_KEY: &str = "tel:encoder_version";

/// Current encoder version. Bumped whenever the meaning of a
/// persisted token counter changes — either the encoder itself
/// (char/4 → BPE) or the static baseline values that get summed
/// into `static_baseline_tokens`. Hydrate drops counters whose
/// stored version doesn't match this constant so old units don't
/// silently mix with new ones on the dashboard.
///
/// History:
/// - `bpe-v1`: file-content baseline goes through cl100k_base BPE
///   (`count_tokens`) while response_tokens stays on the char/4
///   heuristic; see `record_call` in server.rs for the asymmetry.
/// - `bpe-v2-baselines-measured`: static-baseline rows replaced
///   with values measured by `bench-baselines` against a real
///   fixture repo. The asserted point estimates that preceded
///   them under-claimed by 2–5×; the version bump triggers a
///   one-time counter reset so cumulative totals don't average
///   across the two regimes.
const ENCODER_VERSION: &str = "bpe-v2-baselines-measured";

/// Sample period for the BPE-vs-heuristic ratio probe on response
/// payloads. Every Nth call schedules a real cl100k_base count of
/// the response text on a `spawn_blocking` thread, accumulating
/// into `response_bpe_real_total` / `response_heuristic_total_on_samples`
/// so a corrected `tokens_saved` can be computed without paying the
/// BPE cost on every call. 64 = ~1.5 % of calls sampled, well-bounded
/// CPU cost even with the off-thread move; the ratio stabilises after
/// a few hundred calls (~5 minutes of agentic activity).
pub const RESPONSE_BPE_SAMPLE_PERIOD: u64 = 64;

/// Don't bother BPE-sampling responses below this byte size — the
/// char/4 heuristic is accurate enough on small balanced payloads
/// that sampling them just adds noise to the ratio without sharpening
/// it. Picked so that fast tools like `code_outline` (typically 200–
/// 800 byte responses) don't pay any sampling cost at all.
pub const RESPONSE_BPE_SAMPLE_MIN_BYTES: usize = 1024;

/// Meta key under which the dedupe set is persisted. Serialised as a
/// JSON array of `[tool_name, key, ts_secs]` triples; on hydrate
/// every triple older than [`DEDUP_TTL_SECS`] is dropped.
const DEDUP_META_KEY: &str = "tel:dedup_v1";

/// Sliding-window TTL for persisted dedupe entries. 24 h matches a
/// typical "I started a fresh session today" boundary — long enough
/// that a `recon serve` restart inside an active workday doesn't
/// re-credit every baseline (which silently inflates lifetime
/// "tokens saved"), short enough that yesterday's exploratory
/// session doesn't suppress today's first call.
const DEDUP_TTL_SECS: i64 = 24 * 60 * 60;

/// Hard cap on the persisted dedupe set. A pathological session
/// (millions of distinct keys) shouldn't silently grow the meta
/// blob unbounded. On overflow we keep the most-recently-stamped
/// entries — those are the ones still well within the TTL window
/// and most likely to fire again.
const DEDUP_MAX_ENTRIES: usize = 50_000;

/// Per-tool baseline cost: what an agent would otherwise have paid using
/// only Read/Grep/Glob.
pub struct Baseline {
    /// MCP tool name.
    pub tool: &'static str,
    /// Point-estimate tokens an agent would consume reaching the same
    /// answer without recon. For migrated tools this is 0 — the
    /// measured per-call number is the truth and the static counter
    /// never accrues.
    pub baseline_tokens: u64,
    /// Lower end of the realistic baseline range. Captures "small-files
    /// repo" / "narrow query" cases. Surfaced on `code_savings` so the
    /// dashboard can show the static figure as a band rather than a
    /// single integer pretending to be exact. 0 for migrated tools.
    pub range_low_tokens: u64,
    /// Upper end of the realistic baseline range. Captures "big-files
    /// repo" / "broad query" cases. Same 0-for-migrated convention.
    pub range_high_tokens: u64,
    /// Estimated wall-time the alternative Read/Grep loop would have
    /// taken per call, in milliseconds. The session receipt + dashboard
    /// surface `(baseline_latency_ms × calls) - actual_latency_micros`
    /// as "time saved" — the gut-feel number that lands harder than
    /// "saved 47 K tokens." Order-of-magnitude estimates are fine; same
    /// rationale as `baseline_tokens`.
    pub baseline_latency_ms: u64,
    /// One-line rationale shown on `code_savings` for at-a-glance trust.
    pub rationale: &'static str,
    /// Reproducible derivation: the formula or measurement procedure
    /// that yields the point estimate. A skeptical reader should be
    /// able to recompute the number from this string — e.g.
    /// `"5 files × 4 KB median × char/4 ≈ 5 000 tok"`. For migrated
    /// tools: `"BPE count of file content actually read"`.
    pub derivation: &'static str,
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
        range_low_tokens: 0,
        range_high_tokens: 0,
        baseline_latency_ms: 200,
        rationale: "measured per-call against the indexed file",
        derivation: "BPE (cl100k_base) count of file content actually read",
    },
    Baseline {
        tool: "code_skeleton",
        baseline_tokens: 0,
        range_low_tokens: 0,
        range_high_tokens: 0,
        baseline_latency_ms: 200,
        rationale: "measured per-call against the indexed file",
        derivation: "BPE (cl100k_base) count of file content actually read",
    },
    Baseline {
        tool: "code_read_symbol",
        baseline_tokens: 0,
        range_low_tokens: 0,
        range_high_tokens: 0,
        baseline_latency_ms: 250,
        rationale: "measured per-call (full-file Read equivalent)",
        derivation: "BPE (cl100k_base) count of file content actually read",
    },
    // ── Static estimates (composite tools, no clean alternative) ───
    // Static-only: 3-tier exact/BM25/fuzzy search has no clean grep
    // alternative — fuzzy and BM25 ranking can't be reproduced by
    // a single grep pass, so a measured baseline would understate
    // the real Read+grep+read-top-N cost.
    Baseline {
        tool: "code_find_symbol",
        baseline_tokens: 27_534,
        range_low_tokens: 19_494,
        range_high_tokens: 52_240,
        baseline_latency_ms: 800,
        rationale: "Grep across repo + read top 2 hits",
        derivation: "bench-baselines (2026-05-01, intel repo, 130 files): \
                     median grep + 2 hit-file reads across 9 symbol variants",
    },
    // Static-only: handler is index-driven (refs table, no grep on
    // the hot path). Computing a measured baseline would require an
    // extra grep pass per call just to size the alternative —
    // doubles the work without changing the answer. Keep static.
    Baseline {
        tool: "code_find_refs",
        baseline_tokens: 5_979,
        range_low_tokens: 1_665,
        range_high_tokens: 38_276,
        baseline_latency_ms: 600,
        rationale: "Grep for symbol name across repo",
        derivation: "bench-baselines (2026-05-01, intel repo): median grep \
                     output across 9 symbol variants spanning common to rare names",
    },
    Baseline {
        tool: "code_find_strings",
        baseline_tokens: 0,
        range_low_tokens: 0,
        range_high_tokens: 0,
        baseline_latency_ms: 400,
        rationale: "measured per-call (sum of grep match-line tokens)",
        derivation: "BPE (cl100k_base) count over the matched-line bytes the agent would have read",
    },
    Baseline {
        tool: "code_search",
        baseline_tokens: 0,
        range_low_tokens: 0,
        range_high_tokens: 0,
        baseline_latency_ms: 500,
        rationale: "measured per-call when grep path is taken; 0 for tantivy/semantic",
        derivation: "BPE (cl100k_base) count of grep-equivalent match bytes; 0 when no grep alternative exists",
    },
    Baseline {
        tool: "code_multi_find",
        baseline_tokens: 0,
        range_low_tokens: 0,
        range_high_tokens: 0,
        baseline_latency_ms: 1000,
        rationale: "measured per-call (sum across all patterns + matches)",
        derivation: "BPE (cl100k_base) count summed across every pattern's grep-equivalent matches",
    },
    Baseline {
        tool: "code_list",
        baseline_tokens: 0,
        range_low_tokens: 0,
        range_high_tokens: 0,
        baseline_latency_ms: 2000,
        rationale: "measured per-call (sum of path + lang label bytes)",
        derivation: "BPE (cl100k_base) count of the `find` / `ls -R` output the agent would have walked",
    },
    Baseline {
        tool: "code_repo_map",
        baseline_tokens: 33_549,
        range_low_tokens: 32_517,
        range_high_tokens: 34_487,
        baseline_latency_ms: 5000,
        rationale: "Read 5 files for orientation",
        derivation: "bench-baselines (2026-05-01, intel repo): file envelope + \
                     5 reads, median across 1/3, 2/3, full repo cuts",
    },
    Baseline {
        tool: "code_path",
        baseline_tokens: 12_219,
        range_low_tokens: 8_543,
        range_high_tokens: 12_327,
        baseline_latency_ms: 2000,
        rationale: "5x chained code_find_refs",
        derivation: "bench-baselines (2026-05-01, intel repo): 5-symbol chain, \
                     median across repo-size cuts",
    },
    Baseline {
        tool: "code_callers",
        baseline_tokens: 15_711,
        range_low_tokens: 5_764,
        range_high_tokens: 50_099,
        baseline_latency_ms: 800,
        rationale: "depth=1 chained ref lookups",
        derivation: "bench-baselines (2026-05-01, intel repo): depth-1 grep + \
                     1-file read across 9 symbol variants",
    },
    Baseline {
        tool: "code_callees",
        baseline_tokens: 15_711,
        range_low_tokens: 5_764,
        range_high_tokens: 50_099,
        baseline_latency_ms: 800,
        rationale: "depth=1 chained ref lookups",
        derivation: "bench-baselines (2026-05-01, intel repo): depth-1 grep + \
                     1-file read across 9 symbol variants",
    },
    Baseline {
        tool: "code_context",
        baseline_tokens: 0,
        range_low_tokens: 0,
        range_high_tokens: 0,
        baseline_latency_ms: 1500,
        rationale: "measured per-call (target file read; floor on the 4-call alternative)",
        derivation: "BPE (cl100k_base) count of target file (lower bound on read_symbol+find_refs+search-tests loop)",
    },
    // Static-only: pure graph traversal (transitive callers + test
    // detector). No file I/O on the hot path, so the only honest
    // measured baseline would require running the alternative
    // grep-of-callers passes per call — that doubles work without
    // changing the answer. Static stays.
    Baseline {
        tool: "code_impact",
        baseline_tokens: 14_960,
        range_low_tokens: 7_837,
        range_high_tokens: 14_960,
        baseline_latency_ms: 3000,
        rationale: "transitive callers + test grep + analysis",
        derivation: "bench-baselines (2026-05-01, intel repo): 3× chained refs + \
                     test-file grep, median across 2 symbol variants",
    },
    // Static-only: pure connected-components computation over the
    // cached graph. No file I/O, no grep — the alternative cost
    // (orientation = repo_map + 5 file reads) is real but unmeasurable
    // from this handler without doing exactly that extra work.
    Baseline {
        tool: "code_subsystems",
        baseline_tokens: 39_706,
        range_low_tokens: 38_674,
        range_high_tokens: 40_644,
        baseline_latency_ms: 4000,
        rationale: "repo_map + 5 file reads",
        derivation: "bench-baselines (2026-05-01, intel repo): repo_map + 5 \
                     extra reads, median across repo-size cuts",
    },
    // Static-only: index-only lookup over a cluster's symbol metadata.
    // Same reasoning as `code_subsystems` — measuring the alternative
    // (`ls + cat top-N files`) requires the extra reads.
    Baseline {
        tool: "code_subsystem",
        baseline_tokens: 26_151,
        range_low_tokens: 26_151,
        range_high_tokens: 26_151,
        baseline_latency_ms: 1500,
        rationale: "directory listing + reads",
        derivation: "bench-baselines (2026-05-01, intel repo): first-directory \
                     ls + 4 reads (point estimate; sim is shape-invariant)",
    },
    // Operator/system tools — not exposed via MCP as of v0.4. They
    // still get [`ToolCounter`] entries so CLI invocations
    // (`recon stats`, `recon savings show`) can record latency / call
    // counts, but their baseline credit is zero since users — not
    // agents — invoke them.
    Baseline {
        tool: "code_stats",
        baseline_tokens: 0,
        range_low_tokens: 0,
        range_high_tokens: 0,
        baseline_latency_ms: 0,
        rationale: "CLI/operator tool, not agent-facing",
        derivation: "no credit — operator-invoked",
    },
    Baseline {
        tool: "code_reindex",
        baseline_tokens: 0,
        range_low_tokens: 0,
        range_high_tokens: 0,
        baseline_latency_ms: 0,
        rationale: "system operation, no agent alternative",
        derivation: "no credit — system operation",
    },
    Baseline {
        tool: "code_savings",
        baseline_tokens: 0,
        range_low_tokens: 0,
        range_high_tokens: 0,
        baseline_latency_ms: 0,
        rationale: "CLI/operator tool, not agent-facing",
        derivation: "no credit — operator-invoked",
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
    /// Persisted across restarts: each entry stores its creation
    /// timestamp so [`hydrate_from_store`] can drop entries older
    /// than [`DEDUP_TTL_SECS`]. Same logical purpose as before
    /// (gate the *first* baseline credit per `(tool, key)`); the
    /// timestamp converts the gate from process-scoped to
    /// 24 h-sliding-window-scoped.
    dedup: DashMap<(&'static str, CompactString), i64>,

    /// Per-repo calibrated baselines loaded from SQLite `meta` on startup.
    /// Keyed by tool name, valued by the median token count from the local
    /// `bench-baselines` run. When present, `baseline_for_local(tool)`
    /// returns this value instead of the static `BASELINES` entry — giving
    /// large repos an honest 20–80× higher baseline that matches their
    /// actual alternative cost. Populated by the background calibration
    /// task spawned after `index_repo()` (issue #29).
    local_baselines: parking_lot::RwLock<AHashMap<String, u64>>,

    /// Sequence counter that selects which calls receive the BPE
    /// sample. Wraps cleanly at u64 max — overflow takes ~580 years
    /// at 1 M calls / sec.
    response_bpe_seq: AtomicU64,
    /// Number of calls on which a BPE sample was actually taken.
    /// Equal to `floor(seq / RESPONSE_BPE_SAMPLE_PERIOD)` minus any
    /// calls that arrived without response text (defensive None
    /// branch in [`Self::record`]).
    response_bpe_samples: AtomicU64,
    /// Sum of cl100k_base BPE token counts across every sampled
    /// response. Numerator of the corrected-vs-heuristic ratio.
    response_bpe_real_total: AtomicU64,
    /// Sum of `estimate_tokens` heuristic counts across the SAME
    /// sampled responses. Denominator of the ratio. Tracking it
    /// alongside the BPE total — instead of dividing by sample count
    /// — lets the ratio reflect whatever response-size mix the
    /// actual workload produces, not the unweighted call frequency.
    response_heuristic_total_on_samples: AtomicU64,
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
            local_baselines: parking_lot::RwLock::new(AHashMap::new()),
            response_bpe_seq: AtomicU64::new(0),
            response_bpe_samples: AtomicU64::new(0),
            response_bpe_real_total: AtomicU64::new(0),
            response_heuristic_total_on_samples: AtomicU64::new(0),
        }
    }

    /// Returns `true` if this is the first time within the dedupe
    /// window that a baseline has been credited for `(tool, key)`,
    /// `false` thereafter. Caller credits the full baseline only on
    /// the first occurrence and zero baseline on every repeat —
    /// see #25 for the per-tool key table.
    ///
    /// Window is [`DEDUP_TTL_SECS`] sliding, persisted across
    /// restarts via [`Self::flush_dedup_to_store`]. Atomic via
    /// `DashMap::insert` returning `None` only on first write — no
    /// mutex on the hot path.
    pub fn should_credit_baseline(&self, tool: &'static str, key: &str) -> bool {
        self.dedup
            .insert((tool, CompactString::new(key)), now_unix_secs() as i64)
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

    /// Take a one-in-[`RESPONSE_BPE_SAMPLE_PERIOD`] BPE sample on
    /// `response_text` and accumulate the sample into the rolling
    /// ratio. Always called alongside [`Self::record`]; calling
    /// `record` without `sample_response` is fine (the ratio just
    /// stays at its current value), but `sample_response` is
    /// pointless without a paired `record`.
    ///
    /// **Off-thread**: when sampling fires, the BPE encode is moved
    /// to a `spawn_blocking` thread and this function returns
    /// immediately. Without this, even a 1-in-64 sample landed a
    /// 1–5 ms spike on a tool's response path; with it, the only
    /// hot-path cost is the bounded clone of the response prefix.
    ///
    /// **Skips small responses**: payloads below
    /// [`RESPONSE_BPE_SAMPLE_MIN_BYTES`] don't sample at all —
    /// char/4 is already accurate on small balanced strings, and
    /// the ratio is dominated by the long-tail responses where it
    /// actually matters. Returns immediately, advancing no counters.
    pub fn sample_response(self: &Arc<Self>, response_text: &str, heuristic_tokens: u64) {
        if response_text.len() < RESPONSE_BPE_SAMPLE_MIN_BYTES {
            return;
        }
        let seq = self.response_bpe_seq.fetch_add(1, Ordering::Relaxed);
        if !seq.is_multiple_of(RESPONSE_BPE_SAMPLE_PERIOD) {
            return;
        }
        // Bounded clone of the prefix actually used by the encode —
        // never the whole response. `count_tokens_capped` only ever
        // reads the first `COUNT_TOKENS_CAP_BYTES`, so cloning past
        // that boundary would waste bytes on every sample.
        let cap = recon_search::tokens::COUNT_TOKENS_CAP_BYTES;
        let mut cut = response_text.len().min(cap);
        while cut > 0 && !response_text.is_char_boundary(cut) {
            cut -= 1;
        }
        let owned: String = response_text[..cut].to_string();
        let total_len = response_text.len();
        let me = Arc::clone(self);
        // Best-effort fire-and-forget. Failures (runtime shutting
        // down) silently skip the sample — the ratio stays where it
        // was, no harm done.
        tokio::task::spawn_blocking(move || {
            let head_tokens = recon_search::tokens::count_tokens(&owned) as u64;
            let bpe = if total_len <= owned.len() || owned.is_empty() {
                head_tokens
            } else {
                head_tokens.saturating_mul(total_len as u64) / owned.len() as u64
            };
            me.response_bpe_real_total.fetch_add(bpe, Ordering::Relaxed);
            me.response_heuristic_total_on_samples
                .fetch_add(heuristic_tokens, Ordering::Relaxed);
            me.response_bpe_samples.fetch_add(1, Ordering::Relaxed);
        });
    }

    /// Rolling BPE-vs-heuristic ratio for response payloads. Returns
    /// 1.0 when no samples have been collected yet (interpretation:
    /// "trust the heuristic until proven otherwise"). Otherwise the
    /// numerator / denominator are both sums-of-tokens across the
    /// same sampled responses, so the ratio reflects the workload's
    /// actual mix rather than unweighted call frequency.
    ///
    /// Typical values for code: ~0.85–0.93 (real BPE counts ~7–15 %
    /// fewer tokens than `len/4` for code). Multiplying the
    /// heuristic `response_tokens` by this ratio yields an estimate
    /// of what the BPE-real total would have been on the unsampled
    /// calls — closes the unit asymmetry between the (BPE) baseline
    /// and the (heuristic) response in `tokens_saved`.
    pub fn response_bpe_ratio(&self) -> f64 {
        let bpe = self.response_bpe_real_total.load(Ordering::Acquire);
        let heur = self
            .response_heuristic_total_on_samples
            .load(Ordering::Acquire);
        if heur == 0 {
            1.0
        } else {
            bpe as f64 / heur as f64
        }
    }

    /// Number of BPE samples taken so far. Useful for
    /// dashboards/`code_savings --explain` so a viewer can see how
    /// much weight to put on the ratio (1 sample = noisy, 1 000 = trustworthy).
    pub fn response_bpe_sample_count(&self) -> u64 {
        self.response_bpe_samples.load(Ordering::Acquire)
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
                        .fetch_add(self.baseline_for_local(tool), Ordering::Relaxed);
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

    // ── Per-repo local baselines (issue #29) ─────────────────────────────────

    /// Meta key prefix for per-repo calibrated baselines.
    const LOCAL_BASELINE_PREFIX: &'static str = "baselines_local:";

    /// Meta key for the calibration version stamp. Format:
    /// `{file_count}:{unix_timestamp}`. Used by the staleness check to
    /// decide whether to re-run calibration.
    const LOCAL_BASELINE_VERSION_KEY: &'static str = "baselines_local:_version";

    /// Look up the baseline for `tool`, preferring the per-repo local
    /// calibration over the static `BASELINES` table. Returns 0 for
    /// migrated tools (their `BASELINES` entry is 0 and they should
    /// never have a local override — they use per-call measurement).
    pub fn baseline_for_local(&self, tool: &str) -> u64 {
        // Migrated tools always return 0 — they use measured baselines.
        let static_val = baseline_for(tool);
        if static_val == 0 {
            // Check if this is a known migrated tool (has a BASELINES entry
            // with baseline_tokens == 0). If so, return 0 unconditionally.
            if BASELINES
                .iter()
                .any(|b| b.tool == tool && b.baseline_tokens == 0)
            {
                return 0;
            }
        }
        // Check local override first.
        if let Some(local) = self.local_baselines.read().get(tool) {
            return *local;
        }
        static_val
    }

    /// Hydrate local baselines from the SQLite `meta` table. Called once
    /// during `hydrate_from_store`. Non-fatal: missing keys just mean
    /// calibration hasn't run yet (session 1 behavior).
    fn hydrate_local_baselines(&self, store: &Store) {
        let mut map = AHashMap::new();
        for b in BASELINES {
            // Only load overrides for non-migrated tools.
            if b.baseline_tokens == 0 {
                continue;
            }
            let key = format!("{}{}", Self::LOCAL_BASELINE_PREFIX, b.tool);
            if let Ok(Some(val_str)) = store.get_meta(&key) {
                if let Ok(val) = val_str.parse::<u64>() {
                    map.insert(b.tool.to_string(), val);
                }
            }
        }
        if !map.is_empty() {
            debug!(count = map.len(), "telemetry: hydrated local baselines");
        }
        *self.local_baselines.write() = map;
    }

    /// Persist a set of calibrated baselines to the SQLite `meta` table.
    /// Called by the background calibration task after a successful run.
    pub fn persist_local_baselines(store: &Store, results: &[(&str, u64)], file_count: usize) {
        for &(tool, tokens) in results {
            let key = format!("{}{}", Self::LOCAL_BASELINE_PREFIX, tool);
            if let Err(e) = store.set_meta(&key, &tokens.to_string()) {
                warn!(tool, %e, "calibration: failed to persist baseline");
            }
        }
        let version = format!("{}:{}", file_count, now_unix_secs());
        if let Err(e) = store.set_meta(Self::LOCAL_BASELINE_VERSION_KEY, &version) {
            warn!(%e, "calibration: failed to persist version stamp");
        }
    }

    /// Check whether calibration is stale and should re-run. Returns
    /// `true` when: (1) no version stamp exists, or (2) the current
    /// file count differs from the stored count by > 25%.
    pub fn calibration_is_stale(store: &Store, current_file_count: usize) -> bool {
        let version = match store.get_meta(Self::LOCAL_BASELINE_VERSION_KEY) {
            Ok(Some(v)) => v,
            _ => return true, // No calibration yet.
        };
        let stored_count: usize = version
            .split(':')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if stored_count == 0 {
            return true;
        }
        let delta = (current_file_count as f64 - stored_count as f64).abs() / stored_count as f64;
        delta > 0.25
    }

    /// Hot-reload local baselines after a background calibration completes.
    /// Called from the calibration task so the current session immediately
    /// benefits without requiring a restart.
    pub fn reload_local_baselines(&self, store: &Store) {
        self.hydrate_local_baselines(store);
    }

    /// Whether local baselines are populated (for `code_savings` display).
    pub fn has_local_baselines(&self) -> bool {
        !self.local_baselines.read().is_empty()
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
    ///
    /// Encoder-version handling: if the persisted [`ENCODER_VERSION_KEY`]
    /// does not match [`ENCODER_VERSION`], every tool's persisted token
    /// counters (`response_tokens`, `static_baseline_tokens`,
    /// `measured_baseline_tokens`) are zeroed before hydrate. `calls`
    /// and `latency_micros_total` carry over unchanged since their
    /// units are stable across encoder revisions. This costs the user
    /// one cumulative reset on upgrade, which is the honest move —
    /// silently mixing char/4 history with BPE-from-here on the
    /// dashboard would be the wrong trade.
    pub fn hydrate_from_store(self: &Arc<Self>, store: &Store) {
        let current_version = match store.get_meta(ENCODER_VERSION_KEY) {
            Ok(Some(s)) => s,
            Ok(None) => String::new(),
            Err(e) => {
                warn!(%e, "telemetry: encoder-version read failed; treating as upgrade");
                String::new()
            }
        };
        let drop_token_counters = current_version != ENCODER_VERSION;
        if drop_token_counters {
            debug!(
                stored = %current_version,
                expected = ENCODER_VERSION,
                "telemetry: encoder version mismatch — dropping persisted token counters",
            );
        }

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
                Ok(mut snapshot) => {
                    if drop_token_counters {
                        snapshot.response_tokens = 0;
                        snapshot.static_baseline_tokens = 0;
                        snapshot.measured_baseline_tokens = 0;
                    }
                    counter.hydrate(&snapshot);
                }
                Err(e) => warn!(
                    tool = name,
                    %e,
                    "telemetry: meta parse failed; tool counter starts at zero"
                ),
            }
        }

        self.hydrate_dedup_from_store(store);
        self.hydrate_local_baselines(store);
    }

    /// Load the persisted dedupe set, dropping entries older than
    /// [`DEDUP_TTL_SECS`]. Best-effort: a missing or corrupt blob
    /// just leaves the in-memory set empty (the worst case is one
    /// session's worth of double-credit, which is what the
    /// pre-persisted code did unconditionally on every restart).
    ///
    /// Tool names are matched against `BASELINES` to recover their
    /// `&'static str` identity — the dedupe map is keyed on
    /// `&'static str` so the lookup must round-trip through the
    /// canonical interned name rather than a freshly allocated
    /// String.
    fn hydrate_dedup_from_store(&self, store: &Store) {
        let raw = match store.get_meta(DEDUP_META_KEY) {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(e) => {
                warn!(%e, "telemetry: dedup meta read failed; starting empty");
                return;
            }
        };
        // Wire format: array of [tool, key, ts] triples. Tuples are
        // shorter than maps and (importantly) deserialize directly
        // into Rust tuples without needing a struct.
        let entries: Vec<(String, String, i64)> = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                warn!(%e, "telemetry: dedup meta parse failed; starting empty");
                return;
            }
        };
        let cutoff = (now_unix_secs() as i64).saturating_sub(DEDUP_TTL_SECS);
        let mut kept = 0usize;
        let mut dropped_stale = 0usize;
        let mut dropped_unknown = 0usize;
        for (tool_name, key, ts) in entries {
            if ts < cutoff {
                dropped_stale += 1;
                continue;
            }
            let tool: Option<&'static str> = BASELINES
                .iter()
                .find(|b| b.tool == tool_name)
                .map(|b| b.tool);
            match tool {
                Some(t) => {
                    self.dedup.insert((t, CompactString::new(&key)), ts);
                    kept += 1;
                }
                None => {
                    // Tool removed/renamed since the persist —
                    // ignore. No reason to credit a baseline for a
                    // tool that no longer exists.
                    dropped_unknown += 1;
                }
            }
        }
        debug!(
            kept,
            dropped_stale, dropped_unknown, "telemetry: dedup hydrated"
        );
    }

    /// Persist the dedupe set under [`DEDUP_META_KEY`]. Bounds the
    /// payload at [`DEDUP_MAX_ENTRIES`] by keeping the
    /// most-recently-stamped entries — those are the ones still
    /// well within the TTL window and most likely to fire again.
    fn flush_dedup_to_store(&self, store: &Store) -> Result<(), String> {
        let mut entries: Vec<(&'static str, String, i64)> = self
            .dedup
            .iter()
            .map(|e| {
                let (tool, key) = e.key();
                (*tool, key.to_string(), *e.value())
            })
            .collect();

        if entries.len() > DEDUP_MAX_ENTRIES {
            // Keep the freshest entries: sort by timestamp descending,
            // truncate. The dropped tail is closer to the TTL boundary
            // and worth the least baseline-credit suppression.
            entries.sort_by_key(|(_, _, ts)| std::cmp::Reverse(*ts));
            entries.truncate(DEDUP_MAX_ENTRIES);
        }

        let raw = serde_json::to_string(&entries).map_err(|e| format!("dedup serialize: {e}"))?;
        store
            .set_meta(DEDUP_META_KEY, &raw)
            .map_err(|e| format!("dedup set_meta: {e}"))?;
        Ok(())
    }

    /// Persist lifetime counters to the SQLite `meta` table. Holds the
    /// `flush_guard` mutex only while snapshotting — disk I/O happens
    /// without the lock so concurrent flushes don't serialize behind
    /// each other's `set_meta` calls. Called from a `tokio::spawn` so
    /// the hot path doesn't block on disk I/O.
    pub fn flush_to_store(&self, store: &Store) {
        // Snapshot phase — under lock. Cheap (one atomic load per
        // counter) and bounds the critical section to memcpy speed.
        // Tool names are `&'static str`, so the snapshot list does
        // no string allocation.
        let snapshots: Vec<(&'static str, CounterSnapshot)> = {
            let _g = self.flush_guard.lock();
            self.tools
                .iter()
                .filter_map(|(name, counter)| {
                    let snap = counter.snapshot();
                    // Skip empty counters — keeps `meta` pristine for
                    // tools that have never been used.
                    if snap.calls == 0 {
                        None
                    } else {
                        Some((*name, snap))
                    }
                })
                .collect()
        };

        // Write phase — outside the lock. A second concurrent flush
        // can run in parallel; SQLite's own WAL serialises the writes.
        let mut errors = 0;
        for (name, snapshot) in &snapshots {
            let key = format!("tel:tool:{name}");
            let raw = match serde_json::to_string(snapshot) {
                Ok(s) => s,
                Err(e) => {
                    warn!(tool = %name, %e, "telemetry: serialize failed");
                    errors += 1;
                    continue;
                }
            };
            if let Err(e) = store.set_meta(&key, &raw) {
                warn!(tool = %name, %e, "telemetry: meta write failed");
                errors += 1;
            }
        }
        // Stamp the encoder version so the next `hydrate_from_store`
        // can tell whether the persisted token counters were written
        // under the current encoder regime. Idempotent — writing the
        // same value every flush is fine.
        if let Err(e) = store.set_meta(ENCODER_VERSION_KEY, ENCODER_VERSION) {
            warn!(%e, "telemetry: encoder-version write failed");
            errors += 1;
        }
        // Persist the dedupe set so a `recon serve` restart inside
        // the TTL window doesn't re-credit every baseline. Best-
        // effort: a write failure is logged but doesn't block other
        // counter writes. The next flush will retry.
        if let Err(e) = self.flush_dedup_to_store(store) {
            warn!(%e, "telemetry: dedup write failed");
            errors += 1;
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
        // Composite tools keep a non-zero static baseline. Pin to
        // ">0" rather than to specific integers — the BASELINES
        // table values are derived from `bench-baselines` against a
        // real fixture and rebenching can change them; the property
        // we care about is that lookup returns the table value, not
        // any particular number.
        let repo_map = baseline_for("code_repo_map");
        let find_sym = baseline_for("code_find_symbol");
        assert!(repo_map > 0, "code_repo_map baseline must be non-zero");
        assert!(find_sym > 0, "code_find_symbol baseline must be non-zero");
        // Cross-check: the lookup matches what the BASELINES table
        // declares for that tool. Catches a typo'd name in either side.
        let table_repo_map = BASELINES
            .iter()
            .find(|b| b.tool == "code_repo_map")
            .unwrap()
            .baseline_tokens;
        assert_eq!(repo_map, table_repo_map);
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
        let per_call = baseline_for("code_find_symbol");
        // First 9 calls accumulate without firing the flush trigger.
        for _ in 0..(FLUSH_THRESHOLD - 1) {
            assert!(!t.record("code_find_symbol", Duration::from_millis(2), 400, None));
        }
        // 10th call hits the threshold and returns true.
        assert!(t.record("code_find_symbol", Duration::from_millis(2), 400, None));
        let agg = t.aggregate();
        assert_eq!(agg.calls, 10);
        assert_eq!(agg.response_tokens, 4000);
        // 10 × baseline_for(code_find_symbol). Derived from the
        // table rather than a hardcoded constant so a future
        // bench-baselines rerun doesn't regress this test.
        assert_eq!(agg.static_baseline_tokens, 10 * per_call);
        assert_eq!(agg.measured_baseline_tokens, 0);
        assert_eq!(agg.tokens_saved(), (10 * per_call).saturating_sub(4000));
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
        // Only the first call accrued the static baseline. Pulled
        // from BASELINES rather than hardcoded so a bench rerun
        // doesn't regress this test.
        let per_call = baseline_for("code_find_symbol");
        assert_eq!(agg.static_baseline_tokens, per_call);
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

    /// Persisted dedupe round-trip + TTL drop.
    /// (a) Two `should_credit_baseline` calls in one process credit then suppress.
    /// (b) After flush + fresh `Telemetry`, hydrate restores the suppress.
    /// (c) An entry forged with an old timestamp is dropped on hydrate
    ///     so the same key credits again — closes the lifetime
    ///     "tokens saved" inflation hole.
    #[test]
    fn dedup_persists_across_restart_with_ttl_drop() {
        let store = Store::open_memory().unwrap();

        // Process 1: credit a fresh key, then verify the second call
        // is suppressed.
        let t = Arc::new(Telemetry::new());
        assert!(t.should_credit_baseline("code_outline", "src/lib.rs"));
        assert!(!t.should_credit_baseline("code_outline", "src/lib.rs"));
        // Drive a flush so the dedupe set lands in SQLite. We can't
        // call flush_to_store directly without a populated tool
        // counter making the path non-empty, so populate one and flush.
        t.record("code_outline", Duration::from_millis(1), 100, Some(1000));
        t.flush_to_store(&store);

        // Process 2: brand-new Telemetry, hydrate from the same store.
        // The key MUST still be marked credited (no double-credit on
        // restart inside the TTL window).
        let t2 = Arc::new(Telemetry::new());
        t2.hydrate_from_store(&store);
        assert!(
            !t2.should_credit_baseline("code_outline", "src/lib.rs"),
            "key persisted before restart must remain suppressed"
        );

        // Now forge a stale entry: write a dedupe blob whose only
        // timestamp is well past the TTL. Hydrate must drop it,
        // freeing the key to credit again.
        let stale_ts = (now_unix_secs() as i64) - DEDUP_TTL_SECS - 60;
        let stale_blob = serde_json::to_string(&vec![(
            "code_outline".to_string(),
            "src/old_file.rs".to_string(),
            stale_ts,
        )])
        .unwrap();
        store.set_meta(DEDUP_META_KEY, &stale_blob).unwrap();

        let t3 = Arc::new(Telemetry::new());
        t3.hydrate_from_store(&store);
        assert!(
            t3.should_credit_baseline("code_outline", "src/old_file.rs"),
            "stale entry must be dropped on hydrate so the key credits anew"
        );
    }

    /// Sampled-BPE ratio probe: every Nth call schedules a real BPE
    /// count of the response text alongside the heuristic count.
    /// Verifies that
    /// (a) only every Nth call advances `response_bpe_samples`
    ///     (after the spawn_blocking tasks have completed),
    /// (b) the ratio reflects BPE/heuristic on the sampled subset,
    /// (c) `response_bpe_ratio()` returns 1.0 when no samples yet,
    /// (d) responses below `RESPONSE_BPE_SAMPLE_MIN_BYTES` never sample.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sample_response_collects_every_nth_call() {
        let t = Arc::new(Telemetry::new());
        // Pre-sample: ratio defaults to 1.0 ("trust the heuristic").
        assert_eq!(t.response_bpe_ratio(), 1.0);
        assert_eq!(t.response_bpe_sample_count(), 0);

        // Tiny responses (below the size threshold) must not
        // advance the sequence counter at all — we don't even
        // pretend to sample them.
        for _ in 0..32 {
            t.sample_response("short", 1);
        }
        assert_eq!(t.response_bpe_sample_count(), 0);

        // Drive RESPONSE_BPE_SAMPLE_PERIOD * 3 calls with a payload
        // safely above the size threshold. Exactly 3 samples should
        // accumulate after the spawn_blocking tasks land.
        let response = "fn add(a: i32, b: i32) -> i32 { a + b }\n".repeat(40);
        assert!(response.len() >= RESPONSE_BPE_SAMPLE_MIN_BYTES);
        let heuristic = recon_search::tokens::estimate_tokens(&response) as u64;
        let calls = (RESPONSE_BPE_SAMPLE_PERIOD * 3) as usize;
        for _ in 0..calls {
            t.sample_response(&response, heuristic);
        }
        // Wait for the fire-and-forget BPE tasks to update the
        // counters. spawn_blocking is fast (<5 ms each) but not
        // synchronous; poll briefly with a generous deadline.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if t.response_bpe_sample_count() == 3 {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "expected 3 samples within 5 s, got {}",
                    t.response_bpe_sample_count()
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // Ratio reflects BPE/heuristic on the SAMPLED subset.
        // Direction depends on payload shape: punctuation-dense
        // synthetic snippets like the test fixture tokenize into
        // MORE BPE tokens than `len/4` (each `(`, `:`, `,`, `->` is
        // typically its own token), so the ratio for code-heavy
        // payloads can exceed 1.0. Real-world MCP responses sit
        // closer to 1.0 since they mix code with prose. Sanity-
        // bound widely so the test catches obvious breakage
        // (zero / NaN / negative) without tripping on encoding
        // realities.
        let ratio = t.response_bpe_ratio();
        assert!(
            (0.3..=3.0).contains(&ratio),
            "ratio={ratio} outside sane sanity band [0.3, 3.0]"
        );
        assert!(ratio.is_finite() && ratio > 0.0);
    }

    /// Encoder-version upgrade path: a meta blob persisted under an
    /// older encoder (no `tel:encoder_version` key) must hydrate with
    /// token counters zeroed while `calls` and `latency_micros_total`
    /// carry over. After a fresh flush stamps the current version,
    /// the next hydrate carries everything through unchanged.
    ///
    /// This is the load-bearing piece for the BPE-swap upgrade — if
    /// it silently misbehaves, every existing user's first
    /// post-upgrade dashboard view shows mixed-units totals.
    #[test]
    fn hydrate_drops_token_counters_on_encoder_version_upgrade() {
        let store = Store::open_memory().unwrap();

        // Simulate a pre-upgrade state: a CounterSnapshot blob
        // persisted under the old encoder, with NO encoder_version key.
        let legacy = CounterSnapshot {
            calls: 42,
            response_tokens: 12_000,
            static_baseline_tokens: 50_000,
            measured_baseline_tokens: 30_000,
            latency_micros_total: 99_999,
        };
        store
            .set_meta(
                "tel:tool:code_outline",
                &serde_json::to_string(&legacy).unwrap(),
            )
            .unwrap();
        // sanity: no version key persisted yet.
        assert!(store.get_meta(ENCODER_VERSION_KEY).unwrap().is_none());

        // First hydrate: token counters must drop, calls/latency keep.
        let t = Arc::new(Telemetry::new());
        t.hydrate_from_store(&store);
        let after_upgrade = t.tools.get("code_outline").unwrap().snapshot();
        assert_eq!(
            after_upgrade.calls, 42,
            "calls preserved across encoder upgrade"
        );
        assert_eq!(
            after_upgrade.latency_micros_total, 99_999,
            "latency preserved across encoder upgrade"
        );
        assert_eq!(after_upgrade.response_tokens, 0, "response_tokens dropped");
        assert_eq!(
            after_upgrade.static_baseline_tokens, 0,
            "static_baseline_tokens dropped"
        );
        assert_eq!(
            after_upgrade.measured_baseline_tokens, 0,
            "measured_baseline_tokens dropped"
        );

        // Now record a new call (under the new encoder regime) and flush.
        // The flush stamps `tel:encoder_version` so the next hydrate is
        // a same-version round-trip.
        t.record("code_outline", Duration::from_millis(1), 100, Some(2400));
        t.flush_to_store(&store);
        assert_eq!(
            store.get_meta(ENCODER_VERSION_KEY).unwrap().as_deref(),
            Some(ENCODER_VERSION),
            "flush must stamp the encoder version"
        );

        // Second hydrate: same-version, all fields carry over unchanged.
        let snap_before_second_hydrate = t.tools.get("code_outline").unwrap().snapshot();
        let t2 = Arc::new(Telemetry::new());
        t2.hydrate_from_store(&store);
        let after_round_trip = t2.tools.get("code_outline").unwrap().snapshot();
        assert_eq!(
            after_round_trip.calls, snap_before_second_hydrate.calls,
            "same-version hydrate preserves calls"
        );
        assert_eq!(
            after_round_trip.response_tokens, snap_before_second_hydrate.response_tokens,
            "same-version hydrate preserves response_tokens"
        );
        assert_eq!(
            after_round_trip.measured_baseline_tokens,
            snap_before_second_hydrate.measured_baseline_tokens,
            "same-version hydrate preserves measured_baseline_tokens"
        );
    }
}
