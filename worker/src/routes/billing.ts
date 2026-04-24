import { Hono, type Context } from "hono";
import {
  createOrder,
  createPlan,
  createSubscription,
  cancelSubscription,
  verifyWebhookSignature,
} from "../lib/razorpay";
import { getTierConfig, getTierPrice, type Currency } from "../lib/tiers";
import { requireAuth } from "../middleware/auth";
import { clientIp, rateLimit } from "../middleware/ratelimit";
import {
  CheckoutBody,
  SubscribeBody,
  parseBody,
  RazorpayWebhookBody,
} from "../schemas";
import type { AuthUser, Env } from "../types";

type AuthedEnv = { Bindings: Env; Variables: { user: AuthUser } };

export const billingRoutes = new Hono<AuthedEnv>();

/**
 * POST /v1/billing/checkout — legacy one-time purchase flow.
 *
 * Kept for backwards compat and as an escape hatch; new UI calls /subscribe.
 * Not exercised by the dashboard anymore. Can be removed once we're sure no
 * cached clients are still hitting it.
 */
billingRoutes.post(
  "/checkout",
  requireAuth,
  rateLimit<AuthedEnv>(
    "RL_CHECKOUT",
    (c) => c.get("user")?.id ?? clientIp(c),
    60,
  ),
  async (c) => {
    const user = c.get("user");
    const db = c.env.RECON_DB;

    let rawBody: unknown;
    try {
      rawBody = await c.req.json();
    } catch {
      return c.json({ error: "body must be valid JSON" }, 400);
    }
    const parsed = parseBody(CheckoutBody, rawBody);
    if (!parsed.ok) return c.json(parsed.error, 400);

    const tierConfig = getTierConfig(parsed.value.tier);
    // Legacy path only ever shipped USD one-time orders. Hard-pin to USD
    // here; anyone who needs a different currency should use /subscribe.
    const usdPrice = getTierPrice(tierConfig.name, "USD");
    if (!usdPrice) {
      return c.json({ error: "This tier cannot be purchased online" }, 400);
    }

    const receipt = `recon_${user.id.slice(0, 8)}_${Date.now()}`;
    const order = await createOrder(
      c.env.RAZORPAY_KEY_ID,
      c.env.RAZORPAY_KEY_SECRET,
      usdPrice.amount,
      "USD",
      receipt,
      { user_id: user.id, tier: tierConfig.name },
    );

    await db
      .prepare(
        `INSERT INTO payments (user_id, razorpay_order_id, amount_paise, currency, status, tier)
         VALUES (?, ?, ?, 'USD', 'created', ?)`,
      )
      .bind(user.id, order.id, usdPrice.amount, tierConfig.name)
      .run();

    return c.json({
      order_id: order.id,
      amount: usdPrice.amount,
      currency: "USD",
      key_id: c.env.RAZORPAY_KEY_ID,
      tier: tierConfig.name,
      price_display: usdPrice.display,
    });
  },
);

/**
 * POST /v1/billing/subscribe — start a recurring monthly subscription.
 *
 * Creates a Razorpay subscription against the tier's Plan (lazily created
 * the first time we see that tier) and returns Razorpay's hosted-checkout
 * `short_url`. The client redirects the browser there; Razorpay handles
 * card entry + the first charge, then fires `subscription.activated` back
 * to our webhook, which flips the user's tier.
 *
 * Refuses if the user already has an active subscription — upgrade/downgrade
 * between tiers is a separate flow we haven't built yet (v0.1 ships with
 * cancel-and-resubscribe only).
 */
billingRoutes.post(
  "/subscribe",
  requireAuth,
  rateLimit<AuthedEnv>(
    "RL_CHECKOUT",
    (c) => c.get("user")?.id ?? clientIp(c),
    60,
  ),
  async (c) => {
    const user = c.get("user");
    const db = c.env.RECON_DB;

    let rawBody: unknown;
    try {
      rawBody = await c.req.json();
    } catch {
      return c.json({ error: "body must be valid JSON" }, 400);
    }
    const parsed = parseBody(SubscribeBody, rawBody);
    if (!parsed.ok) return c.json(parsed.error, 400);

    // Currency resolution. Explicit body override wins. Otherwise default
    // from Cloudflare's IP-geolocation header: IN → INR (so UPI AutoPay +
    // Net Banking eNACH work natively), everywhere else → USD. Tests and
    // local dev lack the cf.country field, so fall back to USD there too.
    //
    // PPP guard: INR prices are intentionally lower (purchasing-power
    // parity for India). A non-Indian user claiming `currency: "INR"` in
    // the POST body would effectively pay ~75% less than intended. Block
    // that explicitly — the UI already hides the INR toggle for non-IN
    // visitors, so this is the server-side backstop for curl/scripted
    // requests. Missing `cf` (tests, local dev) is treated as non-IN.
    const callerCountry = getCfCountry(c.req.raw);
    if (parsed.value.currency === "INR" && callerCountry !== "IN") {
      return c.json(
        {
          error:
            "INR pricing is only available to subscribers in India. Please subscribe in USD.",
        },
        403,
      );
    }
    const currency: Currency =
      parsed.value.currency ?? resolveDefaultCurrency(c.req.raw);

    const tierConfig = getTierConfig(parsed.value.tier);
    const price = getTierPrice(tierConfig.name, currency);
    if (!price) {
      return c.json(
        {
          error: `Tier ${tierConfig.name} is not priced in ${currency}`,
        },
        400,
      );
    }

    // Refuse to stack subscriptions — except for rows where the user has
    // already clicked Cancel. Those have `cancel_at_period_end = 1` which
    // means "no further renewals"; the billing period still runs out, but
    // from the user's POV they're free to start a new plan. Blocking that
    // leaves them confused ("I cancelled — why is it telling me to cancel
    // again?") which is the exact bug this guard previously caused.
    //
    // The old sub keeps its current period_end (honor-until-end still
    // stands) and the webhook will transition it to `cancelled` when
    // Razorpay's renewal attempt would have been.
    const existing = await db
      .prepare(
        `SELECT id, razorpay_subscription_id, tier, status, cancel_at_period_end
         FROM subscriptions
         WHERE user_id = ?
           AND status IN ('created','authenticated','active','pending','halted')
           AND cancel_at_period_end = 0
         LIMIT 1`,
      )
      .bind(user.id)
      .first();
    if (existing) {
      return c.json(
        {
          error:
            "You already have an active subscription. Cancel it from the dashboard before subscribing to a different tier.",
          existing_tier: existing.tier,
          existing_status: existing.status,
        },
        409,
      );
    }

    // Resolve (or lazily create) the Razorpay plan for this (tier, currency).
    const planId = await ensurePlanForTier(
      db,
      c.env.RAZORPAY_KEY_ID,
      c.env.RAZORPAY_KEY_SECRET,
      tierConfig.name,
      currency,
      price.amount,
    );

    // 120 cycles = 10 years of monthly charges. Effectively unbounded but
    // Razorpay requires a finite total_count.
    const sub = await createSubscription(
      c.env.RAZORPAY_KEY_ID,
      c.env.RAZORPAY_KEY_SECRET,
      {
        plan_id: planId,
        total_count: 120,
        customer_notify: true,
        notes: {
          user_id: user.id,
          tier: tierConfig.name,
          currency,
        },
      },
    );

    // Insert a preliminary subscriptions row. The webhook will promote it
    // to 'active' once the first charge lands. Until then, status='created'
    // blocks a second /subscribe call from this user.
    await db
      .prepare(
        `INSERT INTO subscriptions
           (user_id, razorpay_subscription_id, tier, status)
         VALUES (?, ?, ?, 'created')`,
      )
      .bind(user.id, sub.id, tierConfig.name)
      .run();

    return c.json({
      subscription_id: sub.id,
      short_url: sub.short_url,
      tier: tierConfig.name,
      currency,
      price_display: price.display,
    });
  },
);

/**
 * POST /v1/billing/cancel — schedule cancel-at-cycle-end on the user's
 * active subscription.
 *
 * Honor-until-period-end: access stays on the current tier until
 * `current_period_end`, no further charges happen, and the hourly cron
 * downgrades the user's api_keys to Free once `expires_at` passes.
 */
billingRoutes.post(
  "/cancel",
  requireAuth,
  rateLimit<AuthedEnv>(
    "RL_CHECKOUT",
    (c) => c.get("user")?.id ?? clientIp(c),
    60,
  ),
  async (c) => {
    const user = c.get("user");
    const db = c.env.RECON_DB;

    const sub = await db
      .prepare(
        `SELECT id, razorpay_subscription_id, tier, status,
                current_period_end, cancel_at_period_end
         FROM subscriptions
         WHERE user_id = ? AND status IN ('authenticated','active','pending','halted')
         ORDER BY created_at DESC LIMIT 1`,
      )
      .bind(user.id)
      .first<{
        id: string;
        razorpay_subscription_id: string | null;
        tier: string;
        status: string;
        current_period_end: string | null;
        cancel_at_period_end: number;
      }>();

    if (!sub) {
      return c.json(
        { error: "No active subscription to cancel" },
        404,
      );
    }

    if (sub.cancel_at_period_end) {
      return c.json({
        status: "already_scheduled",
        access_until: sub.current_period_end,
      });
    }

    if (!sub.razorpay_subscription_id) {
      return c.json(
        { error: "Subscription not yet registered with Razorpay" },
        409,
      );
    }

    // Ask Razorpay to stop renewals at the end of the current cycle.
    // This does NOT refund or end access immediately.
    await cancelSubscription(
      c.env.RAZORPAY_KEY_ID,
      c.env.RAZORPAY_KEY_SECRET,
      sub.razorpay_subscription_id,
      true,
    );

    await db
      .prepare(
        `UPDATE subscriptions
         SET cancel_at_period_end = 1,
             cancelled_at = datetime('now')
         WHERE id = ?`,
      )
      .bind(sub.id)
      .run();

    return c.json({
      status: "scheduled",
      access_until: sub.current_period_end,
      tier: sub.tier,
    });
  },
);

/**
 * POST /v1/billing/webhook — Razorpay event webhook.
 *
 * Handles:
 *   - payment.captured           (legacy one-time orders; unchanged)
 *   - subscription.activated     (first charge succeeded — grant tier)
 *   - subscription.charged       (renewal charge — extend expires_at)
 *   - subscription.cancelled     (cycle-end reached after cancel request)
 *   - subscription.halted        (repeated charge failures — preserve access until paid period ends)
 *   - subscription.completed     (total_count reached — same semantics as cancelled)
 *
 * Idempotent for payment events via payment_events(razorpay_payment_id PK).
 * Subscription-only events (cancelled, halted, completed) don't have a
 * payment_id; we rely on the subscription row's status monotonically
 * progressing — replays set the same state and are harmless.
 */
billingRoutes.post(
  "/webhook",
  rateLimit("RL_WEBHOOK", (c) => clientIp(c), 60),
  async (c) => {
    const body = await c.req.text();
    const signature = c.req.header("X-Razorpay-Signature");
    if (!signature) return c.json({ error: "Missing signature" }, 400);

    const valid = await verifyWebhookSignature(
      body,
      signature,
      c.env.RAZORPAY_WEBHOOK_SECRET,
    );
    if (!valid) return c.json({ error: "Invalid signature" }, 400);

    let eventRaw: unknown;
    try {
      eventRaw = JSON.parse(body);
    } catch {
      return c.json({ error: "webhook body is not JSON" }, 400);
    }
    const parsed = parseBody(RazorpayWebhookBody, eventRaw);
    if (!parsed.ok) return c.json(parsed.error, 400);
    const event = parsed.value;
    const db = c.env.RECON_DB;

    switch (event.event) {
      case "payment.captured":
        return handlePaymentCaptured(c, event, db);
      case "subscription.activated":
      case "subscription.charged":
        return handleSubscriptionCharged(c, event, db);
      case "subscription.cancelled":
      case "subscription.completed":
        return handleSubscriptionCancelled(c, event, db);
      case "subscription.halted":
        return handleSubscriptionHalted(c, event, db);
      default:
        return c.json({ status: "ignored", event: event.event });
    }
  },
);

/** GET /v1/billing/portal — subscription status + next billing info. */
billingRoutes.get("/portal", requireAuth, async (c) => {
  const user = c.get("user");
  const db = c.env.RECON_DB;

  const sub = await db
    .prepare(
      `SELECT tier, status, current_period_start, current_period_end,
              cancel_at_period_end, cancelled_at
       FROM subscriptions
       WHERE user_id = ?
       ORDER BY created_at DESC LIMIT 1`,
    )
    .bind(user.id)
    .first<{
      tier: string;
      status: string;
      current_period_start: string | null;
      current_period_end: string | null;
      cancel_at_period_end: number;
      cancelled_at: string | null;
    }>();

  const latestPayment = await db
    .prepare(
      `SELECT tier, status, created_at FROM payments
       WHERE user_id = ? AND status = 'captured'
       ORDER BY created_at DESC LIMIT 1`,
    )
    .bind(user.id)
    .first();

  return c.json({
    tier: user.tier,
    tier_config: getTierConfig(user.tier),
    subscription: sub
      ? {
          tier: sub.tier,
          status: sub.status,
          current_period_start: sub.current_period_start,
          current_period_end: sub.current_period_end,
          cancel_at_period_end: !!sub.cancel_at_period_end,
          cancelled_at: sub.cancelled_at,
        }
      : null,
    latest_payment: latestPayment
      ? {
          tier: latestPayment.tier,
          status: latestPayment.status,
          date: latestPayment.created_at,
        }
      : null,
  });
});

// ────────────────────────────────────────────────────────────────────────────
// Internal webhook handlers — one per event shape.
// Keeping these separate from the dispatch switch makes each handler's
// happy-path short enough to read end-to-end.
// ────────────────────────────────────────────────────────────────────────────

type WebhookCtx = Context<AuthedEnv>;

async function handlePaymentCaptured(
  c: WebhookCtx,
  event: RazorpayWebhookBody,
  db: D1Database,
) {
  const payment = event.payload.payment?.entity;
  if (!payment?.order_id) {
    return c.json({ status: "ignored", reason: "no order_id on payment" });
  }

  const firstTime = await db
    .prepare(
      `INSERT INTO payment_events (razorpay_payment_id, event_type, razorpay_order_id)
       VALUES (?, ?, ?)
       ON CONFLICT(razorpay_payment_id) DO NOTHING
       RETURNING razorpay_payment_id`,
    )
    .bind(payment.id, event.event, payment.order_id)
    .first();

  if (!firstTime) {
    console.warn(
      `webhook retry for payment_id=${payment.id} — already processed`,
    );
    return c.json({ status: "already_processed", payment_id: payment.id });
  }

  const paymentRow = await db
    .prepare("SELECT user_id, tier FROM payments WHERE razorpay_order_id = ?")
    .bind(payment.order_id)
    .first<{ user_id: string; tier: string }>();

  if (!paymentRow) {
    console.warn(
      `webhook for unknown order_id=${payment.order_id} — no payments row`,
    );
    return c.json({ status: "unknown_order", order_id: payment.order_id });
  }

  const newLimits = JSON.stringify(getTierConfig(paymentRow.tier).limits);

  await db.batch([
    db
      .prepare(
        `UPDATE payments SET razorpay_payment_id = ?, status = 'captured'
         WHERE razorpay_order_id = ?`,
      )
      .bind(payment.id, payment.order_id),
    db
      .prepare(
        "UPDATE users SET tier = ?, updated_at = datetime('now') WHERE id = ?",
      )
      .bind(paymentRow.tier, paymentRow.user_id),
    db
      .prepare(
        `UPDATE api_keys SET tier = ?, limits_json = ?
         WHERE user_id = ? AND revoked_at IS NULL`,
      )
      .bind(paymentRow.tier, newLimits, paymentRow.user_id),
    db
      .prepare(
        "UPDATE payment_events SET processed_at = datetime('now') WHERE razorpay_payment_id = ?",
      )
      .bind(payment.id),
  ]);

  return c.json({ status: "ok", payment_id: payment.id });
}

async function handleSubscriptionCharged(
  c: WebhookCtx,
  event: RazorpayWebhookBody,
  db: D1Database,
) {
  const sub = event.payload.subscription?.entity;
  if (!sub) {
    return c.json({ status: "ignored", reason: "no subscription entity" });
  }

  // Locate our row.
  const row = await db
    .prepare(
      `SELECT id, user_id, tier FROM subscriptions
       WHERE razorpay_subscription_id = ?`,
    )
    .bind(sub.id)
    .first<{ id: string; user_id: string; tier: string }>();

  if (!row) {
    console.warn(`subscription webhook for unknown sub_id=${sub.id}`);
    return c.json({ status: "unknown_subscription", subscription_id: sub.id });
  }

  const periodStart = sub.current_start
    ? isoFromUnix(sub.current_start)
    : null;
  const periodEnd = sub.current_end ? isoFromUnix(sub.current_end) : null;
  const limits = JSON.stringify(getTierConfig(row.tier).limits);

  await db.batch([
    db
      .prepare(
        `UPDATE subscriptions
         SET status = 'active',
             current_period_start = COALESCE(?, current_period_start),
             current_period_end = COALESCE(?, current_period_end)
         WHERE id = ?`,
      )
      .bind(periodStart, periodEnd, row.id),
    db
      .prepare(
        "UPDATE users SET tier = ?, updated_at = datetime('now') WHERE id = ?",
      )
      .bind(row.tier, row.user_id),
    db
      .prepare(
        `UPDATE api_keys SET tier = ?, limits_json = ?, expires_at = ?
         WHERE user_id = ? AND revoked_at IS NULL`,
      )
      .bind(row.tier, limits, periodEnd, row.user_id),
  ]);

  return c.json({
    status: "ok",
    subscription_id: sub.id,
    period_end: periodEnd,
  });
}

async function handleSubscriptionCancelled(
  c: WebhookCtx,
  event: RazorpayWebhookBody,
  db: D1Database,
) {
  const sub = event.payload.subscription?.entity;
  if (!sub) {
    return c.json({ status: "ignored", reason: "no subscription entity" });
  }

  // Do NOT expire api_keys here — the user paid for the current cycle and
  // keeps access until current_period_end. The hourly cron will downgrade
  // once that time passes. We only flip the subscription row's status so
  // /portal reflects reality and future charges don't happen.
  await db
    .prepare(
      `UPDATE subscriptions
       SET status = 'cancelled',
           cancelled_at = COALESCE(cancelled_at, datetime('now'))
       WHERE razorpay_subscription_id = ?`,
    )
    .bind(sub.id)
    .run();

  return c.json({ status: "ok", subscription_id: sub.id });
}

async function handleSubscriptionHalted(
  c: WebhookCtx,
  event: RazorpayWebhookBody,
  db: D1Database,
) {
  const sub = event.payload.subscription?.entity;
  if (!sub) {
    return c.json({ status: "ignored", reason: "no subscription entity" });
  }

  // Razorpay has stopped trying to charge after repeated failures. Access
  // continues until the already-paid current_period_end, same as cancelled
  // — the user just can't re-activate without a fresh card.
  await db
    .prepare(
      `UPDATE subscriptions
       SET status = 'halted'
       WHERE razorpay_subscription_id = ?`,
    )
    .bind(sub.id)
    .run();

  return c.json({ status: "ok", subscription_id: sub.id });
}

/**
 * Resolve a Razorpay plan_id for a (tier, currency) pair, creating it in
 * Razorpay (and caching the ID in D1) on first use. Subsequent subscribers
 * to the same (tier, currency) reuse the cached ID; a different currency
 * triggers a fresh plan creation because Razorpay plans are
 * currency-specific.
 *
 * There is a benign race: two simultaneous first-subscribers for the same
 * (tier, currency) could each call createPlan, yielding two orphan plans
 * upstream. The ON CONFLICT(tier, currency) DO NOTHING + re-read pattern
 * settles the race so everyone sees the same plan_id on their next request.
 */
async function ensurePlanForTier(
  db: D1Database,
  keyId: string,
  keySecret: string,
  tierName: string,
  currency: Currency,
  amount: number,
): Promise<string> {
  const cached = await db
    .prepare(
      "SELECT razorpay_plan_id FROM subscription_plans WHERE tier = ? AND currency = ?",
    )
    .bind(tierName, currency)
    .first<{ razorpay_plan_id: string }>();
  if (cached?.razorpay_plan_id) return cached.razorpay_plan_id;

  const plan = await createPlan(keyId, keySecret, {
    tier: tierName,
    amount,
    currency,
    period: "monthly",
    interval: 1,
  });

  await db
    .prepare(
      `INSERT INTO subscription_plans
         (tier, currency, razorpay_plan_id, amount, interval_period, interval_count)
       VALUES (?, ?, ?, ?, 'monthly', 1)
       ON CONFLICT(tier, currency) DO NOTHING`,
    )
    .bind(tierName, currency, plan.id, amount)
    .run();

  // Re-read to resolve the race described above — whoever actually inserted
  // wins, so everyone ends up seeing the same plan_id.
  const winner = await db
    .prepare(
      "SELECT razorpay_plan_id FROM subscription_plans WHERE tier = ? AND currency = ?",
    )
    .bind(tierName, currency)
    .first<{ razorpay_plan_id: string }>();
  return winner?.razorpay_plan_id ?? plan.id;
}

/**
 * Choose the default billing currency for an incoming subscribe request
 * when the body hasn't specified one. Cloudflare's `cf.country` field is
 * populated in production; absent in tests and local dev, in which case
 * we default to USD (matches the pricing page's non-IN default).
 */
function resolveDefaultCurrency(req: Request): Currency {
  return getCfCountry(req) === "IN" ? "INR" : "USD";
}

/**
 * Read Cloudflare's IP-geolocated country code off the request. Returns
 * undefined when the `cf` object is absent (tests, local dev, non-CF
 * proxies) — callers must treat undefined as "not India" for PPP gating
 * so a missing header can't be used to claim INR eligibility.
 */
function getCfCountry(req: Request): string | undefined {
  // Cloudflare attaches a non-standard `cf` object to incoming Requests at
  // runtime. It's typed minimally by workers-types as `CfProperties`; we
  // reach for `country` specifically.
  return (req as unknown as { cf?: { country?: string } }).cf?.country;
}

function isoFromUnix(unixSeconds: number): string {
  return new Date(unixSeconds * 1000).toISOString();
}
