-- Per-repo dimension on the usage_rollups table (v0.3.3+).
--
-- Background: 0009 created `usage_rollups` keyed on (user_id, day). For a
-- single-repo developer that's fine, but Pro tier allows up to 10 repos per
-- account. With the prior shape, every push from the user's CLI MAX-merged
-- into one row, so the dashboard headline ended up showing the high-water
-- mark of the most active single repo on that day — never the cross-repo
-- total. The text made it look like total user activity; it wasn't.
--
-- This migration widens the key to (user_id, repo_fingerprint, day). The
-- worker now SUMs across repo_fingerprint per day on the read path, which
-- gives true cross-repo daily totals without changing the response shape.
-- A per-repo breakdown is a future opt-in.
--
-- Rollout / compat:
--   - Old CLIs (v0.3.2 and earlier) push without `repo_fingerprint`. The
--     worker route binds `''` (empty string) for those — they still land,
--     keyed under the legacy bucket, and SUM picks them up just like a
--     real per-repo row.
--   - Existing rows from the v0.3.2 era keep their data: the rebuild copies
--     them in with `repo_fingerprint = ''`, the same legacy bucket.
--   - New CLIs (v0.3.3+) compute the SHA-256 path fingerprint that
--     `recon init` already registers with /v1/account/repos and include it
--     in the push body. Their pushes land in distinct rows per repo, so
--     SUM is the actual cross-repo total.
--
-- D1 / SQLite specifics:
--   - SQLite cannot ALTER an existing PK in place. Standard table-rebuild
--     dance: create new table, copy rows in, drop old, rename. Wrapped in
--     the migration's implicit transaction so a failure rolls back cleanly.
--   - foreign_keys is enabled on the connection; ON DELETE CASCADE from
--     `users` is preserved on the new table.

CREATE TABLE usage_rollups_v2 (
  user_id           TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  repo_fingerprint  TEXT    NOT NULL DEFAULT '',           -- '' = legacy / pre-v0.3.3
  day               TEXT    NOT NULL,                       -- YYYY-MM-DD UTC
  calls             INTEGER NOT NULL DEFAULT 0,
  response_tokens   INTEGER NOT NULL DEFAULT 0,
  baseline_tokens   INTEGER NOT NULL DEFAULT 0,
  tokens_saved      INTEGER NOT NULL DEFAULT 0,
  latency_micros    INTEGER NOT NULL DEFAULT 0,
  updated_at        TEXT    NOT NULL DEFAULT (datetime('now')),
  PRIMARY KEY (user_id, repo_fingerprint, day)
);

INSERT INTO usage_rollups_v2
       (user_id, repo_fingerprint, day, calls, response_tokens, baseline_tokens, tokens_saved, latency_micros, updated_at)
SELECT  user_id, '',               day, calls, response_tokens, baseline_tokens, tokens_saved, latency_micros, updated_at
FROM   usage_rollups;

DROP TABLE usage_rollups;

ALTER TABLE usage_rollups_v2 RENAME TO usage_rollups;

-- Day-leading covering index. Used by:
--   1. Cron compaction:  DELETE WHERE day < ? (range scan on `day`)
--   2. Optional global "savings across all users today" admin query
-- Single-user pulls hit the new PK already.
CREATE INDEX IF NOT EXISTS idx_usage_rollups_day ON usage_rollups(day);
