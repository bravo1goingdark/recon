/**
 * Razorpay webhook envelope correctness.
 *
 * The legacy one-time `payment.captured` upgrade path was removed in favour
 * of subscriptions (see worker/src/routes/billing.ts) — the lifecycle
 * tests that pin tier-grant + idempotency now live in
 * `subscription-lifecycle.test.ts`.
 *
 * What's left here is the envelope-level contract every event has to
 * satisfy before any handler runs:
 *   - HMAC signature must be present and valid
 *   - body must parse against RazorpayWebhookBody
 *   - unknown event types are 200/ignored so Razorpay doesn't retry-storm
 */

import { describe, expect, it } from "vitest";
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

function envelope(event: string): string {
  return JSON.stringify({
    event,
    // `created_at` is required by the replay-window guard. Use "now" so
    // tests focused on envelope/signature checks aren't tripped by a
    // tangentially-related concern.
    created_at: Math.floor(Date.now() / 1000),
    payload: {
      // Subscription envelope shape — handlers branch on `event.event`,
      // so an `order.paid` body still parses through the same schema.
      subscription: {
        entity: {
          id: "sub_envelope",
          plan_id: "plan_x",
          status: "active",
          current_start: null,
          current_end: null,
        },
      },
    },
  });
}

describe("POST /v1/billing/webhook — envelope contract", () => {
  it("rejects missing signature with 400", async () => {
    await resetDb();
    const body = envelope("subscription.charged");
    const res = await getJson("/v1/billing/webhook", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body,
    });
    expect(res.status).toBe(400);
    expect(res.body).toMatchObject({ error: "Missing signature" });
  });

  it("rejects invalid HMAC signature with 400", async () => {
    await resetDb();
    const body = envelope("subscription.charged");
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

  it("rejects malformed body after signature passes", async () => {
    await resetDb();
    const body = JSON.stringify({ event: "subscription.charged", foo: "bar" });
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

  it("ignores unknown event types with 200", async () => {
    await resetDb();
    const body = envelope("order.paid");
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

  it("rejects events older than the 24-hour replay window AND records audit row", async () => {
    // A captured-and-replayed body from weeks ago has a valid HMAC for the
    // secret's full lifetime — signature alone doesn't bound replay risk.
    // The created_at field caps how stale a delivery can be (24h matches
    // Razorpay's retry envelope so we don't reject legitimate retries).
    await resetDb();
    const stale = Math.floor(Date.now() / 1000) - 25 * 60 * 60; // 25h ago
    const body = JSON.stringify({
      event: "subscription.charged",
      created_at: stale,
      payload: {
        subscription: {
          entity: {
            id: "sub_replay",
            plan_id: "plan_x",
            status: "active",
            current_start: null,
            current_end: null,
          },
        },
      },
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
    expect(res.status).toBe(400);
    expect(res.body).toMatchObject({
      error: "Webhook event outside replay window",
    });

    // Audit trail: dropped event should be queryable in webhook_events_dropped.
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const dropped = await db
      .prepare(
        "SELECT reason FROM webhook_events_dropped WHERE razorpay_subscription_id = ?",
      )
      .bind("sub_replay")
      .first<{ reason: string }>();
    expect(dropped?.reason).toBe("replay_window_exceeded");
  });

  it("accepts events that are stale-but-inside the 24h window (Razorpay retry)", async () => {
    // Razorpay retries deliveries for up to ~24h after a worker failure,
    // and each retry carries the *original* created_at. A stale-by-12h
    // event is legitimate and must NOT be rejected.
    await resetDb();
    const inside = Math.floor(Date.now() / 1000) - 12 * 60 * 60;
    const body = JSON.stringify({
      event: "order.paid", // ignored at dispatch but parses through guard
      created_at: inside,
      payload: {
        subscription: {
          entity: { id: "sub_x", plan_id: "p", status: "active" },
        },
      },
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

  it("rejects events with missing created_at", async () => {
    // Razorpay always sends created_at; a body without it is either a bug
    // upstream or a manually-crafted replay attempt.
    await resetDb();
    const body = JSON.stringify({
      event: "subscription.charged",
      payload: {
        subscription: {
          entity: {
            id: "sub_no_ts",
            plan_id: "plan_x",
            status: "active",
            current_start: null,
            current_end: null,
          },
        },
      },
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
    expect(res.status).toBe(400);
    expect(res.body).toMatchObject({
      error: "Webhook event missing created_at",
    });
  });

  it("dedupes by X-Razorpay-Event-Id — second delivery returns already_processed", async () => {
    // Razorpay's network occasionally double-fires the same event-id during
    // retries. The webhook_events_seen table guarantees side effects run
    // exactly once per event-id, regardless of how many times Razorpay
    // delivers the body.
    await resetDb();
    const body = envelope("order.paid"); // Unknown event = ignored at dispatch.
    const sig = await hmacHex("test-webhook-secret", body);
    const headers = {
      "Content-Type": "application/json",
      "X-Razorpay-Signature": sig,
      "X-Razorpay-Event-Id": "evt_dedup_test",
    };

    const first = await getJson("/v1/billing/webhook", {
      method: "POST",
      headers,
      body,
    });
    expect(first.status).toBe(200);
    expect(first.body).toMatchObject({ status: "ignored", event: "order.paid" });

    const second = await getJson("/v1/billing/webhook", {
      method: "POST",
      headers,
      body,
    });
    expect(second.status).toBe(200);
    expect(second.body).toMatchObject({
      status: "already_processed",
      event_id: "evt_dedup_test",
    });
  });

  it("ignores payment.captured (legacy path removed) with 200", async () => {
    // Razorpay still emits payment.captured for every subscription charge,
    // but the worker only acts on subscription.* events. The dispatch falls
    // through to the default branch and 200s so Razorpay doesn't retry.
    await resetDb();
    const body = JSON.stringify({
      event: "payment.captured",
      created_at: Math.floor(Date.now() / 1000),
      payload: {
        payment: {
          entity: {
            id: "pay_x",
            order_id: "order_x",
            amount: 300,
            currency: "USD",
          },
        },
      },
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
    expect(res.body).toMatchObject({
      status: "ignored",
      event: "payment.captured",
    });
  });
});
