-- Subscription lifecycle: cancel-at-period-end + plan cache.
--
-- Background: the initial schema shipped with a `subscriptions` table but no
-- code path ever wrote to it — billing was one-time Razorpay Orders. This
-- migration adds the columns and tables needed for recurring subscriptions
-- with honor-until-period-end cancellation semantics.
--
-- - subscriptions.cancel_at_period_end: 1 when the user has cancelled but
--   the billing period hasn't ended yet. Service continues until
--   current_period_end, then the hourly cron downgrades them to Free.
-- - subscriptions.cancelled_at: timestamp of the cancel request (for UX +
--   audit). Distinct from current_period_end, which is when access ends.
-- - subscription_plans: cache of Razorpay plan IDs per tier. Razorpay
--   requires a plan before you can create a subscription; we create each
--   tier's plan lazily on first subscribe and cache the ID here so the
--   second subscriber for the same tier reuses it.

ALTER TABLE subscriptions ADD COLUMN cancel_at_period_end INTEGER NOT NULL DEFAULT 0;
ALTER TABLE subscriptions ADD COLUMN cancelled_at TEXT;

CREATE TABLE IF NOT EXISTS subscription_plans (
  tier              TEXT    PRIMARY KEY,
  razorpay_plan_id  TEXT    NOT NULL UNIQUE,
  amount            INTEGER NOT NULL,
  currency          TEXT    NOT NULL DEFAULT 'USD',
  interval_period   TEXT    NOT NULL DEFAULT 'monthly',
  interval_count    INTEGER NOT NULL DEFAULT 1,
  created_at        TEXT    NOT NULL DEFAULT (datetime('now'))
);

-- Index on subscriptions.status for the hourly downgrade cron, which scans
-- for active subscriptions whose current_period_end has passed.
CREATE INDEX IF NOT EXISTS idx_subs_status_period ON subscriptions(status, current_period_end);

-- Index on api_keys.expires_at so the cron's "find expired keys" query is
-- a range scan, not a full table scan. Also filters tier != 'Free' to skip
-- keys that were never upgraded.
CREATE INDEX IF NOT EXISTS idx_api_keys_expires ON api_keys(expires_at) WHERE expires_at IS NOT NULL;
