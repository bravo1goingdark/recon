-- Idempotency log for Razorpay webhooks.
-- Razorpay retries payment.captured with the same payment_id on network
-- hiccups. Without this table the webhook handler re-runs tier upgrades
-- (plus any future side-effects like emails) on every retry. Keyed by
-- payment_id with a unique constraint so `INSERT OR IGNORE` cheaply
-- detects duplicates in one D1 round trip.
CREATE TABLE IF NOT EXISTS payment_events (
  razorpay_payment_id TEXT    PRIMARY KEY,
  event_type          TEXT    NOT NULL,
  razorpay_order_id   TEXT    NOT NULL,
  received_at         TEXT    NOT NULL DEFAULT (datetime('now')),
  processed_at        TEXT
);
CREATE INDEX IF NOT EXISTS idx_payment_events_order ON payment_events(razorpay_order_id);
