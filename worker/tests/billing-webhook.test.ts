/**
 * Razorpay webhook correctness.
 *
 * Focus: idempotency. Razorpay retries payment.captured with the same
 * payment.id on their network blips. Without the payment_events guard,
 * every retry re-applies the tier upgrade batch — semantically OK today
 * (UPDATEs, not INSERTs) but fragile forever.
 *
 * Each test:
 *   1. Seeds a user + pending payment via resetDb()
 *   2. Computes a valid HMAC signature for the body
 *   3. POSTs to /v1/billing/webhook
 *   4. Asserts DB state after 1st vs 2nd delivery
 */

import { beforeEach, describe, expect, it } from "vitest";
import { env, getJson, resetDb } from "./setup";

async function hmacHex(key: string, body: string): Promise<string> {
  const enc = new TextEncoder();
  const cryptoKey = await crypto.subtle.importKey(
    "raw",
    enc.encode(key),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const sig = await crypto.subtle.sign("HMAC", cryptoKey, enc.encode(body));
  return Array.from(new Uint8Array(sig))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

function webhookEnvelope(opts: {
  paymentId: string;
  orderId: string;
  event?: string;
}): string {
  return JSON.stringify({
    event: opts.event ?? "payment.captured",
    payload: {
      payment: {
        entity: {
          id: opts.paymentId,
          order_id: opts.orderId,
          amount: 2900,
          currency: "USD",
        },
      },
    },
  });
}

async function seedOrder(opts: {
  userId: string;
  orderId: string;
  tier: string;
}): Promise<void> {
  const db = (env as { RECON_DB: D1Database }).RECON_DB;
  await db
    .prepare(
      `INSERT INTO users (id, github_id, github_username, email, tier)
       VALUES (?, ?, ?, NULL, 'Free')`,
    )
    .bind(opts.userId, 42, "alice")
    .run();
  await db
    .prepare(
      `INSERT INTO api_keys (user_id, key_hash, key_prefix, name, tier, limits_json)
       VALUES (?, 'dummyhash', 'sk-recon-abcd', 'Default', 'Free', '{}')`,
    )
    .bind(opts.userId)
    .run();
  await db
    .prepare(
      `INSERT INTO payments (user_id, razorpay_order_id, amount_paise, currency, status, tier)
       VALUES (?, ?, 2900, 'USD', 'created', ?)`,
    )
    .bind(opts.userId, opts.orderId, opts.tier)
    .run();
}

describe("POST /v1/billing/webhook", () => {
  beforeEach(async () => {
    await resetDb();
  });

  it("rejects missing signature with 400", async () => {
    const body = webhookEnvelope({ paymentId: "pay_1", orderId: "order_1" });
    const res = await getJson("/v1/billing/webhook", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body,
    });
    expect(res.status).toBe(400);
    expect(res.body).toMatchObject({ error: "Missing signature" });
  });

  it("rejects invalid HMAC signature with 400", async () => {
    const body = webhookEnvelope({ paymentId: "pay_2", orderId: "order_2" });
    const res = await getJson("/v1/billing/webhook", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Razorpay-Signature": "wrong".repeat(16),
      },
      body,
    });
    expect(res.status).toBe(400);
    expect(res.body).toMatchObject({ error: "Invalid signature" });
  });

  it("rejects malformed webhook body after signature passes", async () => {
    // Valid HMAC but body that doesn't match the expected Razorpay shape.
    const body = JSON.stringify({ event: "payment.captured", foo: "bar" });
    const sig = await hmacHex("test-webhook-secret", body);
    const res = await getJson("/v1/billing/webhook", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Razorpay-Signature": sig,
      },
      body,
    });
    expect(res.status).toBe(400);
    expect(res.body).toMatchObject({ error: "invalid request body" });
  });

  it("ignores non-payment.captured events with 200", async () => {
    const body = webhookEnvelope({
      paymentId: "pay_ignore",
      orderId: "order_ignore",
      event: "order.paid",
    });
    const sig = await hmacHex("test-webhook-secret", body);
    const res = await getJson("/v1/billing/webhook", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Razorpay-Signature": sig,
      },
      body,
    });
    expect(res.status).toBe(200);
    expect(res.body).toMatchObject({ status: "ignored", event: "order.paid" });
  });

  it("upgrades tier on first delivery of payment.captured", async () => {
    await seedOrder({ userId: "user_abc", orderId: "order_ok", tier: "Pro" });

    const body = webhookEnvelope({ paymentId: "pay_ok", orderId: "order_ok" });
    const sig = await hmacHex("test-webhook-secret", body);
    const res = await getJson("/v1/billing/webhook", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Razorpay-Signature": sig,
      },
      body,
    });
    expect(res.status).toBe(200);
    expect(res.body).toMatchObject({ status: "ok", payment_id: "pay_ok" });

    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const user = await db
      .prepare("SELECT tier FROM users WHERE id = ?")
      .bind("user_abc")
      .first();
    expect(user?.tier).toBe("Pro");

    const payment = await db
      .prepare(
        "SELECT status, razorpay_payment_id FROM payments WHERE razorpay_order_id = ?",
      )
      .bind("order_ok")
      .first();
    expect(payment?.status).toBe("captured");
    expect(payment?.razorpay_payment_id).toBe("pay_ok");

    const pe = await db
      .prepare(
        "SELECT event_type, processed_at FROM payment_events WHERE razorpay_payment_id = ?",
      )
      .bind("pay_ok")
      .first();
    expect(pe?.event_type).toBe("payment.captured");
    expect(pe?.processed_at).not.toBeNull();
  });

  it("is idempotent on retry — returns already_processed without side effects", async () => {
    await seedOrder({ userId: "user_idem", orderId: "order_idem", tier: "Pro" });

    const body = webhookEnvelope({
      paymentId: "pay_idem",
      orderId: "order_idem",
    });
    const sig = await hmacHex("test-webhook-secret", body);
    const headers = {
      "Content-Type": "application/json",
      "X-Razorpay-Signature": sig,
    };

    // First delivery → ok
    const first = await getJson("/v1/billing/webhook", {
      method: "POST",
      headers,
      body,
    });
    expect(first.body).toMatchObject({ status: "ok" });

    // Second delivery (retry) → already_processed, no re-run.
    const second = await getJson("/v1/billing/webhook", {
      method: "POST",
      headers,
      body,
    });
    expect(second.status).toBe(200);
    expect(second.body).toMatchObject({
      status: "already_processed",
      payment_id: "pay_idem",
    });

    // Verify DB state: still exactly one payment_events row, tier unchanged.
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const count = await db
      .prepare(
        "SELECT COUNT(*) AS n FROM payment_events WHERE razorpay_payment_id = ?",
      )
      .bind("pay_idem")
      .first();
    expect((count as { n: number }).n).toBe(1);

    const user = await db
      .prepare("SELECT tier FROM users WHERE id = ?")
      .bind("user_idem")
      .first();
    expect(user?.tier).toBe("Pro"); // no accidental re-upgrade
  });

  it("handles webhook for unknown order_id without erroring", async () => {
    const body = webhookEnvelope({
      paymentId: "pay_orphan",
      orderId: "order_orphan",
    });
    const sig = await hmacHex("test-webhook-secret", body);
    const res = await getJson("/v1/billing/webhook", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Razorpay-Signature": sig,
      },
      body,
    });
    // We still 200 so Razorpay stops retrying, but report the unknown.
    expect(res.status).toBe(200);
    expect(res.body).toMatchObject({
      status: "unknown_order",
      order_id: "order_orphan",
    });
  });
});
