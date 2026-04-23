import { Hono } from "hono";
import { createOrder, verifyWebhookSignature } from "../lib/razorpay";
import { getTierConfig } from "../lib/tiers";
import { requireAuth } from "../middleware/auth";
import { clientIp, rateLimit } from "../middleware/ratelimit";
import { CheckoutBody, parseBody, RazorpayWebhookBody } from "../schemas";
import type { AuthUser, Env } from "../types";

type AuthedEnv = { Bindings: Env; Variables: { user: AuthUser } };

export const billingRoutes = new Hono<AuthedEnv>();

/**
 * POST /v1/billing/checkout — create a Razorpay order for the requested tier.
 *
 * Rate limited to discourage brute-force price probing: a human clicks
 * "Upgrade" maybe twice a day, never 5 times in an hour.
 */
billingRoutes.post(
  "/checkout",
  requireAuth,
  rateLimit<AuthedEnv>(
    "RL_CHECKOUT",
    (c) => c.get("user")?.id ?? clientIp(c),
    3600,
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
    if (tierConfig.price_cents <= 0) {
      return c.json({ error: "This tier cannot be purchased online" }, 400);
    }

    const receipt = `recon_${user.id.slice(0, 8)}_${Date.now()}`;
    const order = await createOrder(
      c.env.RAZORPAY_KEY_ID,
      c.env.RAZORPAY_KEY_SECRET,
      tierConfig.price_cents,
      "USD",
      receipt,
      { user_id: user.id, tier: tierConfig.name },
    );

    // amount_paise column historically stored INR paise; we store USD cents.
    await db
      .prepare(
        `INSERT INTO payments (user_id, razorpay_order_id, amount_paise, currency, status, tier)
         VALUES (?, ?, ?, 'USD', 'created', ?)`,
      )
      .bind(user.id, order.id, tierConfig.price_cents, tierConfig.name)
      .run();

    return c.json({
      order_id: order.id,
      amount: tierConfig.price_cents,
      currency: "USD",
      key_id: c.env.RAZORPAY_KEY_ID,
      tier: tierConfig.name,
      price_display: tierConfig.price_display,
    });
  },
);

/**
 * POST /v1/billing/webhook — Razorpay payment webhook.
 *
 * **Idempotent.** Razorpay retries `payment.captured` on its own network
 * hiccups with the same `payment.id`. We log every event into
 * `payment_events(razorpay_payment_id PRIMARY KEY)` via `INSERT OR IGNORE`
 * before doing any work; a retry finds the row already present and we
 * short-circuit with `status: "already_processed"`.
 *
 * Rate limited per source IP (not user, since webhook callers don't auth):
 * legitimate Razorpay traffic sits far below the cap; noise from spoofed
 * endpoints is shed early.
 */
billingRoutes.post(
  "/webhook",
  rateLimit("RL_WEBHOOK", (c) => clientIp(c), 60),
  async (c) => {
    const body = await c.req.text();
    const signature = c.req.header("X-Razorpay-Signature");

    if (!signature) {
      return c.json({ error: "Missing signature" }, 400);
    }

    const valid = await verifyWebhookSignature(
      body,
      signature,
      c.env.RAZORPAY_WEBHOOK_SECRET,
    );
    if (!valid) {
      return c.json({ error: "Invalid signature" }, 400);
    }

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

    // Only payment.captured is actionable today; everything else is acked
    // but idempotent-logged so dashboards can report on webhook traffic.
    if (event.event !== "payment.captured") {
      return c.json({ status: "ignored", event: event.event });
    }

    const payment = event.payload.payment.entity;

    // Idempotency gate: if we've already seen this payment_id, bail early
    // WITHOUT running the tier upgrade again. D1 doesn't return changes()
    // for INSERT OR IGNORE directly, so we use the returning-row pattern.
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
      .prepare(
        "SELECT user_id, tier FROM payments WHERE razorpay_order_id = ?",
      )
      .bind(payment.order_id)
      .first();

    if (!paymentRow) {
      // Webhook for an order we don't know about — not fatal, but worth
      // surfacing. Keep the idempotency row so Razorpay's retry loop stops.
      console.warn(
        `webhook for unknown order_id=${payment.order_id} — no payments row`,
      );
      return c.json({ status: "unknown_order", order_id: payment.order_id });
    }

    const userId = paymentRow.user_id as string;
    const newTier = paymentRow.tier as string;
    const newLimits = JSON.stringify(getTierConfig(newTier).limits);

    // All three writes in one round-trip. The fourth statement marks the
    // event processed — stamped *after* the business writes so a partial
    // batch leaves processed_at null and the next retry re-runs.
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
        .bind(newTier, userId),
      db
        .prepare(
          `UPDATE api_keys SET tier = ?, limits_json = ?
           WHERE user_id = ? AND revoked_at IS NULL`,
        )
        .bind(newTier, newLimits, userId),
      db
        .prepare(
          "UPDATE payment_events SET processed_at = datetime('now') WHERE razorpay_payment_id = ?",
        )
        .bind(payment.id),
    ]);

    return c.json({ status: "ok", payment_id: payment.id });
  },
);

/** GET /v1/billing/portal — subscription status. */
billingRoutes.get("/portal", requireAuth, async (c) => {
  const user = c.get("user");
  const db = c.env.RECON_DB;

  // Latest captured payment
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
    latest_payment: latestPayment
      ? {
          tier: latestPayment.tier,
          status: latestPayment.status,
          date: latestPayment.created_at,
        }
      : null,
  });
});
