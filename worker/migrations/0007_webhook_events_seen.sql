-- Webhook event idempotency.
--
-- Razorpay sends a unique `X-Razorpay-Event-Id` header on every delivery
-- (format `evt_...`). Their network occasionally double-fires the same
-- event-id during retries, and a captured-and-replayed payload remains
-- a valid HMAC for the secret's full lifetime — so signature verification
-- alone doesn't bound replay risk.
--
-- This table is the dedup record. Insert-or-ignore on the event_id PK; if
-- the insert is silently skipped, we know we've already processed this
-- delivery and can return a 2xx without re-running side effects.
--
-- Rows are kept indefinitely (small footprint — 1 per delivery) so the
-- audit trail stays queryable. A separate cleanup cron can prune older
-- rows once we have enough volume to care.

CREATE TABLE IF NOT EXISTS webhook_events_seen (
  event_id     TEXT PRIMARY KEY,
  event_type   TEXT NOT NULL,
  received_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_seen_received ON webhook_events_seen(received_at);
