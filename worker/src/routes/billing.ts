import { Hono } from "hono";
import { createOrder, verifyWebhookSignature } from "../lib/razorpay";
import { getTierConfig } from "../lib/tiers";
import { requireAuth } from "../middleware/auth";
import type { AuthUser, Env } from "../types";

export const billingRoutes = new Hono<{
  Bindings: Env;
  Variables: { user: AuthUser };
}>();

/** POST /v1/billing/checkout — create Razorpay order. */
billingRoutes.post("/checkout", requireAuth, async (c) => {
  const user = c.get("user");
  const db = c.env.RECON_DB;
  const { tier } = await c.req.json<{ tier: string }>();

  const tierConfig = getTierConfig(tier);
  if (tierConfig.price_paise <= 0) {
    return c.json({ error: "This tier cannot be purchased online" }, 400);
  }

  const receipt = `recon_${user.id.slice(0, 8)}_${Date.now()}`;
  const order = await createOrder(
    c.env.RAZORPAY_KEY_ID,
    c.env.RAZORPAY_KEY_SECRET,
    tierConfig.price_paise,
    "INR",
    receipt,
    { user_id: user.id, tier: tierConfig.name },
  );

  // Store payment record
  await db
    .prepare(
      `INSERT INTO payments (user_id, razorpay_order_id, amount_paise, currency, status, tier)
       VALUES (?, ?, ?, 'INR', 'created', ?)`,
    )
    .bind(user.id, order.id, tierConfig.price_paise, tierConfig.name)
    .run();

  return c.json({
    order_id: order.id,
    amount: tierConfig.price_paise,
    currency: "INR",
    key_id: c.env.RAZORPAY_KEY_ID,
    tier: tierConfig.name,
    price_display: tierConfig.price_display,
  });
});

/** POST /v1/billing/webhook — Razorpay payment webhook. */
billingRoutes.post("/webhook", async (c) => {
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

  const event = JSON.parse(body);
  const db = c.env.RECON_DB;

  if (event.event === "payment.captured") {
    const payment = event.payload.payment.entity;
    const orderId = payment.order_id;

    // Find the payment record
    const paymentRow = await db
      .prepare(
        "SELECT user_id, tier FROM payments WHERE razorpay_order_id = ?",
      )
      .bind(orderId)
      .first();

    if (paymentRow) {
      const userId = paymentRow.user_id as string;
      const newTier = paymentRow.tier as string;
      const newLimits = JSON.stringify(getTierConfig(newTier).limits);

      // Update payment status
      await db
        .prepare(
          `UPDATE payments SET razorpay_payment_id = ?, status = 'captured'
           WHERE razorpay_order_id = ?`,
        )
        .bind(payment.id, orderId)
        .run();

      // Upgrade user tier
      await db
        .prepare(
          "UPDATE users SET tier = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(newTier, userId)
        .run();

      // Upgrade all active (non-revoked) API keys
      await db
        .prepare(
          `UPDATE api_keys SET tier = ?, limits_json = ?
           WHERE user_id = ? AND revoked_at IS NULL`,
        )
        .bind(newTier, newLimits, userId)
        .run();
    }
  }

  return c.json({ status: "ok" });
});

/** GET /v1/billing/portal — subscription status. */
billingRoutes.get("/portal", requireAuth, async (c) => {
  const user = c.get("user");
  const db = c.env.RECON_DB;

  // Latest payment
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
