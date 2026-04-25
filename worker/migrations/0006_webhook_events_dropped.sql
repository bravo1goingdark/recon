-- Audit trail for webhook events we deliberately drop instead of acting on.
--
-- Workers `console.warn` vanishes without log forwarding configured, which
-- left us with no way to investigate "why didn't this user get upgraded?".
-- This table is the queryable record for postmortems and the future
-- /v1/admin debug surface.
--
-- Reasons we currently record (kept open-ended on purpose):
--   missing_current_end     Razorpay event has null current_end; refusing
--                           the grant prevents permanent-free-Pro state
--                           (cron skips NULL expires_at by design).
--   unknown_subscription    sub_id not in our table and no recoverable
--                           placeholder_id in notes.
--   subscription_terminal   row is cancelled / completed / expired and a
--                           late `charged` webhook tried to resurrect it.
--   missing_user_id         legacy notes shape with no user_id; cannot
--                           bind the subscription to anyone.
--
-- Webhook still returns 2xx after recording — the row IS the response, so
-- Razorpay does not retry-storm a known-bad event.

CREATE TABLE IF NOT EXISTS webhook_events_dropped (
  id                       TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(16)))),
  event_type               TEXT NOT NULL,
  razorpay_subscription_id TEXT,
  reason                   TEXT NOT NULL,
  payload_json             TEXT NOT NULL,
  dropped_at               TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_dropped_sub ON webhook_events_dropped(razorpay_subscription_id);
CREATE INDEX IF NOT EXISTS idx_dropped_reason ON webhook_events_dropped(reason);
