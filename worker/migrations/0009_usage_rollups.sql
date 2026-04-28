-- Per-day token-savings rollups for the dashboard Savings panel
-- (Pro/Team tier feature, v0.3.2+).
--
-- Background: recon's MCP server tracks per-tool counters in each
-- developer's local `.recon/recon.db` (token-savings telemetry shipped
-- in v0.3.1). For team-level visibility ("how much did engineering
-- save in API tokens this month?") we want a server-side aggregate.
--
-- The CLI pushes one row per UTC day per user via
-- `POST /v1/account/savings`. Pushes are idempotent and monotone:
-- repeated pushes for the same (user, day) MAX-merge each counter so
-- a stale client cannot regress the total. This trades "correct delta
-- accounting" for "cannot lose data on a retry," which matters more
-- when the source of truth is the local DB (not the worker).
--
-- Privacy: the worker only sees six integers per day per user. No
-- code, symbol names, file paths, or query strings travel. Same weight
-- as a SaaS reporting "you made N API calls today." Free tier cannot
-- push (the route returns 402 Payment Required) so the table never
-- accumulates rows for unpaid accounts.
--
-- (user_id, day) PK lets the GET endpoint range-scan a user's series
-- with a simple equality + range predicate that hits the PK directly:
--   WHERE user_id = ? AND day BETWEEN ? AND ?
-- This is the hot read path. The PK is also the natural target for
-- ON CONFLICT idempotent upsert — no separate index needed for writes.
--
-- The (day, user_id) covering index supports the cron compaction job
-- ("delete rollups older than 90 days") with a single index scan
-- bounded by day, no per-row table lookup needed.

CREATE TABLE IF NOT EXISTS usage_rollups (
  user_id           TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  day               TEXT    NOT NULL,                       -- YYYY-MM-DD UTC
  calls             INTEGER NOT NULL DEFAULT 0,
  response_tokens   INTEGER NOT NULL DEFAULT 0,
  baseline_tokens   INTEGER NOT NULL DEFAULT 0,
  tokens_saved      INTEGER NOT NULL DEFAULT 0,
  latency_micros    INTEGER NOT NULL DEFAULT 0,
  updated_at        TEXT    NOT NULL DEFAULT (datetime('now')),
  PRIMARY KEY (user_id, day)
);

-- Day-leading covering index. Used by:
--   1. Cron compaction:  DELETE WHERE day < ? (range scan on `day`)
--   2. Optional global "savings across all users today" admin query
-- Single-user pulls hit the PK already.
CREATE INDEX IF NOT EXISTS idx_usage_rollups_day ON usage_rollups(day);
