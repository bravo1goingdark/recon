/**
 * End-to-end subscription lifecycle: webhook events + scheduled downgrade.
 *
 * Each test follows the same pattern as billing-webhook.test.ts:
 *   1. resetDb() — fresh schema through migration 0003
 *   2. Seed a user + api_key + subscription row
 *   3. POST a signed webhook (or call downgradeExpired() directly)
 *   4. Assert DB state matches the stage of the subscription lifecycle
 *
 * Honor-until-period-end is the contract the user explicitly asked for:
 * cancellation records the intent but keeps service running until
 * current_period_end passes, at which point the hourly cron downgrades
 * the api_keys row. These tests pin both halves of that contract.
 */

import { beforeEach, describe, expect, it } from "vitest";
import { env, getJson, resetDb } from "./setup";
import { downgradeExpired } from "../src/scheduled";

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

function subWebhook(opts: {
  event: string;
  subscriptionId: string;
  status?: string;
  currentStart?: number;
  currentEnd?: number;
}): string {
  return JSON.stringify({
    event: opts.event,
    payload: {
      subscription: {
        entity: {
          id: opts.subscriptionId,
          plan_id: "plan_test",
          status: opts.status ?? "active",
          current_start: opts.currentStart ?? null,
          current_end: opts.currentEnd ?? null,
        },
      },
    },
  });
}

async function seedSubscriber(opts: {
  userId: string;
  githubId: number;
  tier: string;
  subId: string;
  subStatus?: string;
}): Promise<void> {
  const db = (env as { RECON_DB: D1Database }).RECON_DB;
  await db
    .prepare(
      `INSERT INTO users (id, github_id, github_username, email, tier)
       VALUES (?, ?, ?, NULL, ?)`,
    )
    .bind(opts.userId, opts.githubId, `user_${opts.githubId}`, "Free")
    .run();
  await db
    .prepare(
      `INSERT INTO api_keys (user_id, key_hash, key_prefix, name, tier, limits_json)
       VALUES (?, ?, ?, 'Default', 'Free', '{"max_repos":1,"max_files":250,"max_loc":10000}')`,
    )
    .bind(opts.userId, `hash_${opts.userId}`, `sk-recon-${opts.userId.slice(0, 6)}`)
    .run();
  await db
    .prepare(
      `INSERT INTO subscriptions
         (user_id, razorpay_subscription_id, tier, status)
       VALUES (?, ?, ?, ?)`,
    )
    .bind(opts.userId, opts.subId, opts.tier, opts.subStatus ?? "created")
    .run();
}

describe("subscription webhook: subscription.activated / charged", () => {
  beforeEach(async () => {
    await resetDb();
  });

  it("promotes a created subscription to active and stamps expires_at on the api_key", async () => {
    await seedSubscriber({
      userId: "user_act",
      githubId: 101,
      tier: "Pro",
      subId: "sub_activate",
    });

    // Razorpay sends current_end as a Unix second. 30 days from now:
    const periodStart = Math.floor(Date.now() / 1000);
    const periodEnd = periodStart + 30 * 86_400;

    const body = subWebhook({
      event: "subscription.activated",
      subscriptionId: "sub_activate",
      status: "active",
      currentStart: periodStart,
      currentEnd: periodEnd,
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
      status: "ok",
      subscription_id: "sub_activate",
    });

    const db = (env as { RECON_DB: D1Database }).RECON_DB;

    const subRow = await db
      .prepare(
        `SELECT status, current_period_start, current_period_end
         FROM subscriptions WHERE razorpay_subscription_id = ?`,
      )
      .bind("sub_activate")
      .first<{
        status: string;
        current_period_start: string;
        current_period_end: string;
      }>();
    expect(subRow?.status).toBe("active");
    expect(subRow?.current_period_end).not.toBeNull();

    const userRow = await db
      .prepare("SELECT tier FROM users WHERE id = ?")
      .bind("user_act")
      .first();
    expect(userRow?.tier).toBe("Pro");

    const keyRow = await db
      .prepare(
        "SELECT tier, expires_at FROM api_keys WHERE user_id = ?",
      )
      .bind("user_act")
      .first<{ tier: string; expires_at: string | null }>();
    expect(keyRow?.tier).toBe("Pro");
    expect(keyRow?.expires_at).not.toBeNull();
    // expires_at should be ISO-8601 close to period_end
    const expectedISO = new Date(periodEnd * 1000).toISOString();
    expect(keyRow?.expires_at).toBe(expectedISO);
  });

  it("subscription.charged on an active sub extends expires_at", async () => {
    await seedSubscriber({
      userId: "user_renew",
      githubId: 102,
      tier: "Pro",
      subId: "sub_renew",
      subStatus: "active",
    });

    const renewalEnd = Math.floor(Date.now() / 1000) + 60 * 86_400; // 60 days out

    const body = subWebhook({
      event: "subscription.charged",
      subscriptionId: "sub_renew",
      currentEnd: renewalEnd,
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
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const keyRow = await db
      .prepare("SELECT expires_at FROM api_keys WHERE user_id = ?")
      .bind("user_renew")
      .first<{ expires_at: string }>();
    expect(keyRow?.expires_at).toBe(new Date(renewalEnd * 1000).toISOString());
  });

  it("ignores a subscription event for an unknown sub_id without erroring", async () => {
    const body = subWebhook({
      event: "subscription.charged",
      subscriptionId: "sub_nonexistent",
      currentEnd: 1800000000,
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
      status: "unknown_subscription",
      subscription_id: "sub_nonexistent",
    });
  });
});

describe("subscription webhook: subscription.cancelled / halted — honor until period end", () => {
  beforeEach(async () => {
    await resetDb();
  });

  it("subscription.cancelled flips status but KEEPS expires_at intact", async () => {
    await seedSubscriber({
      userId: "user_cancel",
      githubId: 201,
      tier: "Pro",
      subId: "sub_cancel",
      subStatus: "active",
    });

    // Simulate the subscription was activated and has 20 days left.
    const periodEnd = Math.floor(Date.now() / 1000) + 20 * 86_400;
    const periodEndISO = new Date(periodEnd * 1000).toISOString();

    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    await db
      .prepare(
        `UPDATE subscriptions SET current_period_end = ? WHERE razorpay_subscription_id = ?`,
      )
      .bind(periodEndISO, "sub_cancel")
      .run();
    await db
      .prepare(
        `UPDATE api_keys SET tier = 'Pro', expires_at = ? WHERE user_id = ?`,
      )
      .bind(periodEndISO, "user_cancel")
      .run();

    // Deliver the cancel webhook.
    const body = subWebhook({
      event: "subscription.cancelled",
      subscriptionId: "sub_cancel",
      status: "cancelled",
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

    // Subscription row is cancelled + stamped.
    const subRow = await db
      .prepare(
        `SELECT status, cancelled_at, current_period_end
         FROM subscriptions WHERE razorpay_subscription_id = ?`,
      )
      .bind("sub_cancel")
      .first<{ status: string; cancelled_at: string; current_period_end: string }>();
    expect(subRow?.status).toBe("cancelled");
    expect(subRow?.cancelled_at).not.toBeNull();

    // CRUCIAL: api_keys.expires_at is unchanged. Service continues until
    // the paid period ends; the cron will downgrade only after that.
    const keyRow = await db
      .prepare(
        "SELECT tier, expires_at FROM api_keys WHERE user_id = ?",
      )
      .bind("user_cancel")
      .first<{ tier: string; expires_at: string }>();
    expect(keyRow?.tier).toBe("Pro");
    expect(keyRow?.expires_at).toBe(periodEndISO);
  });

  it("subscription.halted marks halted but keeps api_key paid through period_end", async () => {
    await seedSubscriber({
      userId: "user_halt",
      githubId: 202,
      tier: "Pro",
      subId: "sub_halt",
      subStatus: "active",
    });

    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const periodEndISO = new Date(
      (Math.floor(Date.now() / 1000) + 10 * 86_400) * 1000,
    ).toISOString();
    await db
      .prepare(
        `UPDATE api_keys SET tier = 'Pro', expires_at = ? WHERE user_id = ?`,
      )
      .bind(periodEndISO, "user_halt")
      .run();

    const body = subWebhook({
      event: "subscription.halted",
      subscriptionId: "sub_halt",
      status: "halted",
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

    const subRow = await db
      .prepare(
        "SELECT status FROM subscriptions WHERE razorpay_subscription_id = ?",
      )
      .bind("sub_halt")
      .first<{ status: string }>();
    expect(subRow?.status).toBe("halted");

    const keyRow = await db
      .prepare("SELECT tier, expires_at FROM api_keys WHERE user_id = ?")
      .bind("user_halt")
      .first<{ tier: string; expires_at: string }>();
    expect(keyRow?.tier).toBe("Pro");
    expect(keyRow?.expires_at).toBe(periodEndISO);
  });
});

describe("POST /v1/billing/subscribe — cancel-at-period-end unblocks new subscribe", () => {
  beforeEach(async () => {
    await resetDb();
  });

  /**
   * Seed a user + session + active subscription with a specific
   * cancel_at_period_end flag. Returns the session token for
   * auth. No api_key needed — /subscribe doesn't require one.
   */
  async function seedUserWithSub(opts: {
    userId: string;
    sessionToken: string;
    cancel_at_period_end: number;
  }): Promise<void> {
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const tokenHash = await (async () => {
      const enc = new TextEncoder();
      const buf = await crypto.subtle.digest("SHA-256", enc.encode(opts.sessionToken));
      return Array.from(new Uint8Array(buf))
        .map((b) => b.toString(16).padStart(2, "0"))
        .join("");
    })();
    const sessionExpiry = new Date(Date.now() + 86_400_000).toISOString();
    await db
      .prepare(
        "INSERT INTO users (id, github_id, github_username, tier) VALUES (?, ?, ?, 'Pro')",
      )
      .bind(opts.userId, Math.floor(Math.random() * 1_000_000), `user_${opts.userId}`)
      .run();
    await db
      .prepare(
        "INSERT INTO sessions (user_id, token_hash, expires_at) VALUES (?, ?, ?)",
      )
      .bind(opts.userId, tokenHash, sessionExpiry)
      .run();
    const periodEnd = new Date(Date.now() + 20 * 86_400_000).toISOString();
    await db
      .prepare(
        `INSERT INTO subscriptions
           (user_id, razorpay_subscription_id, tier, status,
            current_period_end, cancel_at_period_end)
         VALUES (?, 'sub_block_test', 'Pro', 'active', ?, ?)`,
      )
      .bind(opts.userId, periodEnd, opts.cancel_at_period_end)
      .run();
  }

  it("blocks new /subscribe when there's a not-yet-cancelled active sub (positive guard case)", async () => {
    await seedUserWithSub({
      userId: "user_still_paying",
      sessionToken: "ses_still_paying",
      cancel_at_period_end: 0,
    });

    const res = await getJson("/v1/billing/subscribe", {
      method: "POST",
      headers: {
        Authorization: "Bearer ses_still_paying",
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ tier: "Team", currency: "USD" }),
    });
    expect(res.status).toBe(409);
    expect(res.body).toMatchObject({
      existing_tier: "Pro",
      existing_status: "active",
    });
  });

  it("rejects currency:'INR' with 403 when cf.country is not 'IN' (PPP guard)", async () => {
    // Non-Indian user trying to abuse the INR price (~75% cheaper for PPP
    // reasons) by POSTing `currency: "INR"` directly. The test env's
    // Request has no `cf` object attached, so getCfCountry() returns
    // undefined and the guard fires the same way it would for a non-IN
    // production caller. USD from the same caller still works — proved
    // by the existing "blocks new /subscribe…" test above.
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const tokenHash = await (async () => {
      const enc = new TextEncoder();
      const buf = await crypto.subtle.digest("SHA-256", enc.encode("ses_ppp_probe"));
      return Array.from(new Uint8Array(buf))
        .map((b) => b.toString(16).padStart(2, "0"))
        .join("");
    })();
    const expiry = new Date(Date.now() + 86_400_000).toISOString();
    await db
      .prepare(
        "INSERT INTO users (id, github_id, github_username, tier) VALUES ('user_ppp_probe', 777777, 'ppp_probe', 'Free')",
      )
      .run();
    await db
      .prepare(
        "INSERT INTO sessions (user_id, token_hash, expires_at) VALUES ('user_ppp_probe', ?, ?)",
      )
      .bind(tokenHash, expiry)
      .run();

    const res = await getJson("/v1/billing/subscribe", {
      method: "POST",
      headers: {
        Authorization: "Bearer ses_ppp_probe",
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ tier: "Pro", currency: "INR" }),
    });
    expect(res.status).toBe(403);
    expect(res.body).toMatchObject({
      error: expect.stringContaining("India"),
    });
  });

  it("passes the 409 guard when the existing sub has cancel_at_period_end=1", async () => {
    // This is the bug the user hit: they cancelled Pro, then clicked
    // Subscribe Team and got "cancel from dashboard" anyway. After the
    // fix the 409 no longer matches cancelled-at-period-end rows, so the
    // request reaches Razorpay. In this test env Razorpay isn't mocked,
    // so the real createPlan/createSubscription call against the
    // test-mode key will either succeed or 5xx — either way, the
    // observable here is "status !== 409", which is the whole contract.
    await seedUserWithSub({
      userId: "user_cancelled",
      sessionToken: "ses_cancelled",
      cancel_at_period_end: 1,
    });

    const res = await getJson("/v1/billing/subscribe", {
      method: "POST",
      headers: {
        Authorization: "Bearer ses_cancelled",
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ tier: "Team", currency: "USD" }),
    });
    expect(res.status).not.toBe(409);
  });
});

describe("scheduled cron: downgradeExpired", () => {
  beforeEach(async () => {
    await resetDb();
  });

  it("downgrades keys whose expires_at is in the past", async () => {
    await seedSubscriber({
      userId: "user_past",
      githubId: 301,
      tier: "Pro",
      subId: "sub_past",
      subStatus: "cancelled",
    });

    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    // Stamp the api_key as expired 1 hour ago with Pro tier.
    const pastISO = new Date(Date.now() - 3600 * 1000).toISOString();
    await db
      .prepare(
        `UPDATE api_keys
         SET tier = 'Pro',
             expires_at = ?,
             limits_json = '{"max_repos":10,"max_files":5000,"max_loc":200000}'
         WHERE user_id = ?`,
      )
      .bind(pastISO, "user_past")
      .run();
    await db
      .prepare("UPDATE users SET tier = 'Pro' WHERE id = ?")
      .bind("user_past")
      .run();

    const result = await downgradeExpired(db);
    expect(result.downgraded_keys).toBe(1);
    expect(result.downgraded_users).toBe(1);

    const keyRow = await db
      .prepare(
        "SELECT tier, limits_json, expires_at FROM api_keys WHERE user_id = ?",
      )
      .bind("user_past")
      .first<{ tier: string; limits_json: string; expires_at: string | null }>();
    expect(keyRow?.tier).toBe("Free");
    expect(keyRow?.expires_at).toBeNull();
    const limits = JSON.parse(keyRow?.limits_json ?? "{}");
    expect(limits.max_loc).toBe(10_000); // Free default

    const userRow = await db
      .prepare("SELECT tier FROM users WHERE id = ?")
      .bind("user_past")
      .first<{ tier: string }>();
    expect(userRow?.tier).toBe("Free");
  });

  it("leaves paid keys alone if expires_at is still in the future", async () => {
    await seedSubscriber({
      userId: "user_future",
      githubId: 302,
      tier: "Pro",
      subId: "sub_future",
      subStatus: "active",
    });

    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const futureISO = new Date(Date.now() + 7 * 86_400 * 1000).toISOString();
    await db
      .prepare(
        `UPDATE api_keys SET tier = 'Pro', expires_at = ? WHERE user_id = ?`,
      )
      .bind(futureISO, "user_future")
      .run();
    await db
      .prepare("UPDATE users SET tier = 'Pro' WHERE id = ?")
      .bind("user_future")
      .run();

    const result = await downgradeExpired(db);
    expect(result.downgraded_keys).toBe(0);

    const keyRow = await db
      .prepare("SELECT tier FROM api_keys WHERE user_id = ?")
      .bind("user_future")
      .first<{ tier: string }>();
    expect(keyRow?.tier).toBe("Pro");
  });

  it("is idempotent — second run after first downgrades nothing", async () => {
    await seedSubscriber({
      userId: "user_idem",
      githubId: 303,
      tier: "Pro",
      subId: "sub_idem",
      subStatus: "cancelled",
    });
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    await db
      .prepare(
        `UPDATE api_keys SET tier = 'Pro', expires_at = ? WHERE user_id = ?`,
      )
      .bind(new Date(Date.now() - 60_000).toISOString(), "user_idem")
      .run();

    const first = await downgradeExpired(db);
    expect(first.downgraded_keys).toBe(1);

    const second = await downgradeExpired(db);
    expect(second.downgraded_keys).toBe(0);
  });
});
