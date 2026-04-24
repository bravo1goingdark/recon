-- Currency-per-plan: subscription_plans now keyed by (tier, currency).
--
-- Background: 0003 created subscription_plans with `tier` as the sole primary
-- key. That was fine when every Razorpay plan was USD-denominated, but
-- Indian users paying in USD can't use UPI AutoPay or Net Banking eNACH —
-- those rails are INR-only. We now create two Razorpay plans per tier
-- (one INR, one USD) so users default to the currency their bank supports
-- natively. (tier, currency) must be unique; razorpay_plan_id stays unique
-- because Razorpay issues distinct plan IDs per currency anyway.
--
-- SQLite can't alter a primary key, so we rename + recreate + copy.

ALTER TABLE subscription_plans RENAME TO subscription_plans_old;

CREATE TABLE subscription_plans (
  tier             TEXT    NOT NULL,
  currency         TEXT    NOT NULL,
  razorpay_plan_id TEXT    NOT NULL UNIQUE,
  amount           INTEGER NOT NULL,
  interval_period  TEXT    NOT NULL DEFAULT 'monthly',
  interval_count   INTEGER NOT NULL DEFAULT 1,
  created_at       TEXT    NOT NULL DEFAULT (datetime('now')),
  PRIMARY KEY (tier, currency)
);

-- Preserve whatever plan IDs were already cached. The old table has a
-- currency column (defaulted 'USD' in 0003 + 'INR' in the test-mode
-- deploy that exercised the INR path); carry whatever's there forward
-- into the composite key verbatim.
INSERT INTO subscription_plans
  (tier, currency, razorpay_plan_id, amount, interval_period, interval_count, created_at)
SELECT
  tier, currency, razorpay_plan_id, amount, interval_period, interval_count, created_at
FROM subscription_plans_old;

DROP TABLE subscription_plans_old;
