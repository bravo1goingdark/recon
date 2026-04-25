-- Server-side repo tracking for v0.2.0.
--
-- Background: prior to v0.2 the CLI enforced `max_repos` against a local
-- file at ~/.config/recon/repos.json — a patched binary trivially bypassed
-- that check. This table moves enforcement onto the worker. The CLI
-- registers each `recon init`'d repo by SHA-256 fingerprint of its
-- canonical absolute path; the worker rejects registrations that would
-- exceed the api_key's tier-defined max_repos.
--
-- Fingerprint scheme: lowercase hex SHA-256(canonical_abs_path_utf8).
-- The worker never sees the path, only the digest, so we don't burn data
-- on user filesystem layout. Moving a repo directory burns a slot — the
-- v0.2 ADR documents that tradeoff; git-remote dedup is a v0.4+ option.
--
-- (user_id, fingerprint) is the PK so the ON CONFLICT path can refresh
-- last_seen_at idempotently. ON DELETE CASCADE from users matches the
-- rest of the schema (tearing down a user removes their repos rather
-- than orphaning rows).

CREATE TABLE IF NOT EXISTS user_repos (
  user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  fingerprint   TEXT NOT NULL,
  first_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
  last_seen_at  TEXT NOT NULL DEFAULT (datetime('now')),
  PRIMARY KEY (user_id, fingerprint)
);

-- Single-column index on user_id supports the COUNT(*) check in the
-- conditional INSERT, plus the GET /v1/account/repos listing query.
CREATE INDEX IF NOT EXISTS idx_user_repos_user ON user_repos(user_id);
