-- Measured-baseline columns on usage_rollups (v0.4 measured-savings v0).
--
-- Background: through v0.3.3 the "tokens saved" headline came entirely
-- from the static BASELINES table baked into recon-server — a defensible
-- estimate, but an estimate. The migration plan
-- (`/home/bravo1goingdark/.claude/plans/do-it-lexical-turtle.md`) ships
-- per-call measurement gated behind RECON_MEASURED_BASELINES=1. Each
-- measured call records both the *static* baseline (legacy column) and
-- a *measured* baseline (new columns). The dashboard surfaces both.
--
-- Three new columns:
--   measured_baseline_tokens — sum of measured Read/grep equivalent
--                              token cost on calls where the flag was on.
--   measured_response_tokens — sum of response_tokens for the same
--                              measured-call subset, so the worker can
--                              compute `measured_tokens_saved` without
--                              re-deriving from totals.
--   measured_calls           — count of calls in the measured slice.
--                              Drives the dashboard's
--                              measured/estimated badge ratio.
--
-- Rollout / compat:
--   - Old CLIs (without the new fields): worker validators treat them
--     as optional and bind 0. INSERT lands; new columns stay zero.
--     Dashboard correctly reports the row as 100% estimated.
--   - New CLI → old worker: this migration has not yet shipped, so the
--     worker silently drops the unknown push fields. Estimated columns
--     continue to MAX-merge as before; the measured numbers from that
--     day are dropped on the floor. Acceptable until this migration
--     deploys.
--   - Existing rows from before this migration: ALTER TABLE adds the
--     columns with DEFAULT 0, so all historical data correctly
--     attributes to the estimated track.
--
-- D1 / SQLite specifics:
--   - SQLite supports ALTER TABLE ADD COLUMN with NOT NULL only when a
--     DEFAULT is given (which we always do here). No table rebuild
--     needed — the columns append in place.

ALTER TABLE usage_rollups
  ADD COLUMN measured_baseline_tokens INTEGER NOT NULL DEFAULT 0;

ALTER TABLE usage_rollups
  ADD COLUMN measured_response_tokens INTEGER NOT NULL DEFAULT 0;

ALTER TABLE usage_rollups
  ADD COLUMN measured_calls INTEGER NOT NULL DEFAULT 0;
