-- v0.5: latency-saved column on usage_rollups.
--
-- Token savings is one number; users feel time more than tokens. The
-- session receipt and dashboard now surface "X minutes faster than the
-- Read+Grep loop" alongside "saved K tokens" — same shape as the
-- existing tokens_saved column. The CLI computes
--
--   latency_saved_micros = Σ (baseline_latency_ms × calls × 1000)
--                         - Σ latency_micros_total
--
-- per-tool, clamped at 0, and pushes the daily aggregate. Stored
-- verbatim; the dashboard reads + renders without re-deriving.
--
-- DEFAULT 0 keeps pre-v0.5 CLIs talking to a v0.5 worker safe — they
-- omit the field, the column accepts the absent push as 0, and the
-- monotone MAX-merge on conflict means a fresh push from a v0.5 CLI
-- bumps the same row up to the real number.
--
-- D1 / SQLite: ALTER TABLE ADD COLUMN runs in place on modern SQLite.
-- No table rebuild.

ALTER TABLE usage_rollups
  ADD COLUMN latency_saved_micros INTEGER NOT NULL DEFAULT 0;
