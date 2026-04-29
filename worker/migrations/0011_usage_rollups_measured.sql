-- Measured-baseline restructure of usage_rollups (v0.4 measured-savings).
--
-- The v0.4 dashboard splits the "tokens saved" headline into two
-- mutually-exclusive baseline sources:
--
--   static_baseline_tokens    — credited from the BASELINES table for
--                               composite tools (no clean Read/grep
--                               equivalent). Renamed from the legacy
--                               `baseline_tokens` column for clarity.
--   measured_baseline_tokens  — credited per-call from a real Read/grep
--                               equivalent for migrated handlers
--                               (code_outline, code_skeleton,
--                               code_read_symbol, …).
--
-- Each call accrues exactly one of the two. tokens_saved is computed
-- on the read path as `(static + measured) - response_tokens`,
-- clamped at 0. The legacy `tokens_saved` column stays for now;
-- pushes that include both let the worker validate them against
-- the derived figure.
--
-- D1 / SQLite specifics: SQLite ≥ 3.25 supports both `RENAME COLUMN`
-- and `ADD COLUMN` in place — no table rebuild needed. D1 ships on a
-- modern build, so these statements run as a single migration.
--
-- No back-compat gymnastics: pre-launch, no production rows exist, so
-- the rename + add is straightforward. CLI and worker both ship with
-- the new field names; the wire shape changes in lockstep.

ALTER TABLE usage_rollups RENAME COLUMN baseline_tokens TO static_baseline_tokens;

ALTER TABLE usage_rollups
  ADD COLUMN measured_baseline_tokens INTEGER NOT NULL DEFAULT 0;
