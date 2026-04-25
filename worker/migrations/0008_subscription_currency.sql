-- Track the billing currency on each subscription row.
--
-- /v1/billing/subscribe needs to compare tier + currency between the
-- incoming request and an existing 'created' placeholder to decide
-- whether to resume the same Razorpay sub (same tier+currency) or swap
-- (cancel + recreate). Without storing currency, we'd have to either
-- fetch from Razorpay on every retry or join through subscription_plans,
-- both of which add latency to a hot path.
--
-- Existing rows: production has zero subscription rows at the time of
-- this migration (the legacy test-mode plans were purged when we rotated
-- to live keys), so the DEFAULT 'USD' is a no-op for prod data. The
-- default is kept as a safety net for any future row inserted before the
-- application code starts setting currency explicitly.

ALTER TABLE subscriptions ADD COLUMN currency TEXT NOT NULL DEFAULT 'USD';
