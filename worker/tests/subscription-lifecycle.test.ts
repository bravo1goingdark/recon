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

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
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
  createdAt?: number;
}): string {
  return JSON.stringify({
    event: opts.event,
    // `created_at` defaults to "now" so the replay-window guard passes.
    // Tests that want to exercise the guard pass an explicit value.
    created_at: opts.createdAt ?? Math.floor(Date.now() / 1000),
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

  // Flake at the default 5 s vitest timeout under CI load — the test
  // makes a real Razorpay createPlan/createSubscription call (no mock
  // in this env) and the network round-trip occasionally exceeds 5 s.
  // Bump to 15 s for this case only; passes in <500 ms locally.
  it("passes the 409 guard when the existing sub has cancel_at_period_end=1", { timeout: 15_000 }, async () => {
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

describe("subscription webhook: halted → charged recovery (card update)", () => {
  beforeEach(async () => {
    await resetDb();
  });

  it("subscription.charged after halted re-promotes the sub to active", async () => {
    // Flow: Razorpay charges fail repeatedly → status='halted'. The user
    // updates their card on Razorpay's portal → next charge succeeds → a
    // `subscription.charged` event arrives. The handler must promote the
    // halted row back to 'active' and extend expires_at.
    //
    // This is the recovery path we deliberately do NOT block in the
    // status-guard fix (only cancelled / completed / expired are terminal).
    // The test pins that decision so a future "tighten the guard" change
    // doesn't accidentally strand users with a halted row that no event
    // can rescue.
    await seedSubscriber({
      userId: "user_halt_recover",
      githubId: 601,
      tier: "Pro",
      subId: "sub_halt_recover",
      subStatus: "halted",
    });

    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    // Before recovery: api_keys are still on a previously-paid period_end
    // (honor-until-end after the halt — the handleSubscriptionHalted path
    // doesn't expire the key, the cron downgrades after period_end passes).
    const oldEnd = Math.floor(Date.now() / 1000) + 2 * 86_400;
    const oldEndISO = new Date(oldEnd * 1000).toISOString();
    await db
      .prepare(
        `UPDATE api_keys SET tier = 'Pro', expires_at = ? WHERE user_id = ?`,
      )
      .bind(oldEndISO, "user_halt_recover")
      .run();

    // Card update succeeds — Razorpay retries the charge, fires charged.
    const newEnd = Math.floor(Date.now() / 1000) + 30 * 86_400;
    const body = subWebhook({
      event: "subscription.charged",
      subscriptionId: "sub_halt_recover",
      currentEnd: newEnd,
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
      subscription_id: "sub_halt_recover",
    });

    // Subscription row: halted → active, expires_at extended.
    const subRow = await db
      .prepare(
        "SELECT status, current_period_end FROM subscriptions WHERE razorpay_subscription_id = ?",
      )
      .bind("sub_halt_recover")
      .first<{ status: string; current_period_end: string }>();
    expect(subRow?.status).toBe("active");
    expect(subRow?.current_period_end).toBe(
      new Date(newEnd * 1000).toISOString(),
    );

    // api_keys: tier remains Pro (cascade ran), expires_at extended to new end.
    const keyRow = await db
      .prepare("SELECT tier, expires_at FROM api_keys WHERE user_id = ?")
      .bind("user_halt_recover")
      .first<{ tier: string; expires_at: string }>();
    expect(keyRow?.tier).toBe("Pro");
    expect(keyRow?.expires_at).toBe(new Date(newEnd * 1000).toISOString());
  });
});

describe("billing critical bugs — race + status guard + null current_end", () => {
  beforeEach(async () => {
    await resetDb();
  });

  /**
   * Seed a session-only user (no subscription, no api_key). Used by the
   * /subscribe race test below: the test only needs an authenticated caller,
   * not pre-existing billing state.
   */
  async function seedSession(userId: string, sessionToken: string): Promise<void> {
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const enc = new TextEncoder();
    const buf = await crypto.subtle.digest("SHA-256", enc.encode(sessionToken));
    const tokenHash = Array.from(new Uint8Array(buf))
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");
    const expiry = new Date(Date.now() + 86_400_000).toISOString();
    await db
      .prepare(
        "INSERT INTO users (id, github_id, github_username, tier) VALUES (?, ?, ?, 'Free')",
      )
      .bind(userId, Math.floor(Math.random() * 1_000_000_000), `user_${userId}`)
      .run();
    await db
      .prepare(
        "INSERT INTO sessions (user_id, token_hash, expires_at) VALUES (?, ?, ?)",
      )
      .bind(userId, tokenHash, expiry)
      .run();
  }

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("race: 3 concurrent /subscribe call Razorpay AT MOST once", async () => {
    // Bug: the old SELECT-then-Razorpay-then-INSERT pattern lets concurrent
    // calls all pass the SELECT, all call createSubscription (double-charging
    // the user upstream), then all INSERT duplicate subscription rows.
    //
    // Fix: an atomic INSERT-WHERE-NOT-EXISTS placeholder runs *before* the
    // Razorpay call. Only one of the N wins the placeholder; the rest see
    // it via the WHERE-NOT-EXISTS guard and return 409 without ever touching
    // Razorpay.
    //
    // We stub `fetch` so Razorpay calls (a) succeed deterministically and
    // (b) park long enough that the winner is still in-flight when the
    // losers run their INSERT-WHERE-NOT-EXISTS — which is the exact race
    // window where the bug exists in production.
    //
    // We use 3 concurrent calls because the RL_CHECKOUT rate-limit binding
    // is configured at 3/min in wrangler.toml. Anything above 3 hits 429
    // (which is also a useful defense-in-depth signal but not what this
    // test is pinning).
    await seedSession("user_race", "ses_race");

    const razorpayCalls: { url: string; body: unknown }[] = [];
    const realFetch = globalThis.fetch;
    vi.stubGlobal(
      "fetch",
      async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
        const url =
          typeof input === "string"
            ? input
            : input instanceof URL
              ? input.toString()
              : input.url;
        if (!url.startsWith("https://api.razorpay.com")) {
          return realFetch(input, init);
        }
        const body =
          typeof init?.body === "string" ? JSON.parse(init.body) : null;
        razorpayCalls.push({ url, body });
        // Hold the response long enough that all five /subscribe handlers
        // have run their atomic INSERT-WHERE-NOT-EXISTS before the winner's
        // Razorpay call resolves. 200ms is way longer than D1 statement
        // dispatch (sub-ms) so the losers all see the placeholder.
        await new Promise((r) => setTimeout(r, 200));
        if (url.endsWith("/plans")) {
          return new Response(
            JSON.stringify({
              id: "plan_stub",
              entity: "plan",
              period: "monthly",
              interval: 1,
              item: {
                id: "item_stub",
                name: "recon Pro",
                amount: 300,
                currency: "USD",
              },
            }),
            { status: 200, headers: { "Content-Type": "application/json" } },
          );
        }
        if (url.endsWith("/subscriptions")) {
          return new Response(
            JSON.stringify({
              id: "sub_stub_winner",
              entity: "subscription",
              plan_id: "plan_stub",
              status: "created",
              short_url: "https://rzp.io/i/stub",
              current_start: null,
              current_end: null,
              ended_at: null,
              quantity: 1,
              notes: body?.notes ?? {},
              charge_at: null,
              start_at: null,
              end_at: null,
              auth_attempts: 0,
              total_count: body?.total_count ?? 120,
              paid_count: 0,
              customer_notify: !!body?.customer_notify,
              created_at: Math.floor(Date.now() / 1000),
              has_scheduled_changes: false,
              change_scheduled_at: null,
            }),
            { status: 200, headers: { "Content-Type": "application/json" } },
          );
        }
        return new Response("not stubbed", { status: 500 });
      },
    );

    const launch = () =>
      getJson("/v1/billing/subscribe", {
        method: "POST",
        headers: {
          Authorization: "Bearer ses_race",
          "Content-Type": "application/json",
        },
        body: JSON.stringify({ tier: "Pro", currency: "USD" }),
      });

    const results = await Promise.all([launch(), launch(), launch()]);

    const okCount = results.filter((r) => r.status === 200).length;
    const conflictCount = results.filter((r) => r.status === 409).length;
    // Exactly one wins the placeholder and reaches Razorpay; the other two
    // are blocked by the WHERE NOT EXISTS guard before the upstream call.
    expect(okCount).toBe(1);
    expect(conflictCount).toBe(2);

    // Critical user-impact assertion: Razorpay was called AT MOST once.
    // Under the bug this number is 5 (one per concurrent click), each
    // creating a subscription and an upcoming charge.
    const subscriptionCalls = razorpayCalls.filter((c) =>
      c.url.endsWith("/subscriptions"),
    ).length;
    expect(subscriptionCalls).toBeLessThanOrEqual(1);

    // Exactly one subscriptions row exists, stamped with the Razorpay sub_id.
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const rows = await db
      .prepare(
        "SELECT razorpay_subscription_id, status FROM subscriptions WHERE user_id = ?",
      )
      .bind("user_race")
      .all<{ razorpay_subscription_id: string | null; status: string }>();
    expect(rows.results.length).toBe(1);
    expect(rows.results[0].razorpay_subscription_id).toBe("sub_stub_winner");
  });

  it("status guard: subscription.charged on a cancelled sub does NOT resurrect it", async () => {
    // Bug: handleSubscriptionCharged blindly UPDATEs status='active' and
    // overwrites api_keys.tier + expires_at. Razorpay delivers webhooks at
    // least once, in any order — a delayed `charged` arriving after a
    // `cancelled` would silently flip the user back to Pro and extend
    // service for another billing period they didn't pay for.
    //
    // Fix: the UPDATE filters `WHERE status NOT IN ('cancelled','completed','expired')`.
    // If meta.changes === 0 we skip the cascading user/api_keys updates and
    // return a non-200 reason so the dropped event is auditable.
    await seedSubscriber({
      userId: "user_resurrect",
      githubId: 401,
      tier: "Pro",
      subId: "sub_resurrect",
      subStatus: "cancelled",
    });

    // Pre-state mirrors a freshly cancelled sub still inside its paid period:
    // status=cancelled, expires_at stamped at cycle end (honor-until-end).
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const originalEnd = Math.floor(Date.now() / 1000) + 5 * 86_400;
    const originalEndISO = new Date(originalEnd * 1000).toISOString();
    await db
      .prepare(
        `UPDATE subscriptions
         SET current_period_end = ?,
             cancelled_at = datetime('now')
         WHERE razorpay_subscription_id = ?`,
      )
      .bind(originalEndISO, "sub_resurrect")
      .run();
    // api_keys keep Free tier here (per seedSubscriber default) — the test
    // is about whether a stale `charged` webhook can promote them back.

    // Out-of-order webhook: a `subscription.charged` arrives AFTER cancellation
    // with a "new" period_end far in the future.
    const lateRenewalEnd = Math.floor(Date.now() / 1000) + 90 * 86_400;
    const body = subWebhook({
      event: "subscription.charged",
      subscriptionId: "sub_resurrect",
      currentEnd: lateRenewalEnd,
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

    // Webhook must 2xx so Razorpay stops retrying — but the side-effects
    // must NOT have flipped the user back to active.
    expect(res.status).toBe(200);

    const subRow = await db
      .prepare(
        "SELECT status, current_period_end FROM subscriptions WHERE razorpay_subscription_id = ?",
      )
      .bind("sub_resurrect")
      .first<{ status: string; current_period_end: string }>();
    expect(subRow?.status).toBe("cancelled");
    expect(subRow?.current_period_end).toBe(originalEndISO);

    const userRow = await db
      .prepare("SELECT tier FROM users WHERE id = ?")
      .bind("user_resurrect")
      .first<{ tier: string }>();
    expect(userRow?.tier).toBe("Free");

    const keyRow = await db
      .prepare("SELECT tier, expires_at FROM api_keys WHERE user_id = ?")
      .bind("user_resurrect")
      .first<{ tier: string; expires_at: string | null }>();
    expect(keyRow?.tier).toBe("Free");
    expect(keyRow?.expires_at).toBeNull();

    // Audit row: the dropped event must record reason='subscription_terminal'.
    const dropped = await db
      .prepare(
        "SELECT reason FROM webhook_events_dropped WHERE razorpay_subscription_id = ?",
      )
      .bind("sub_resurrect")
      .first<{ reason: string }>();
    expect(dropped?.reason).toBe("subscription_terminal");
  });

  it("self-heal: webhook with notes.placeholder_id stamps the orphan placeholder", async () => {
    // Defends the recovery path: if /subscribe successfully creates a
    // Razorpay subscription but the post-Razorpay UPDATE fails (D1 hiccup,
    // worker isolate killed, etc), we end up with a placeholder row whose
    // razorpay_subscription_id is NULL. The eventual subscription.activated
    // webhook carries notes.placeholder_id and uses it to find our row,
    // stamp the sub_id, and proceed with the activation cascade.
    //
    // Without this test, a regression in the self-heal lookup would ship
    // silently — the bug would only surface when a real production hiccup
    // hit the UPDATE.
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    await db
      .prepare(
        "INSERT INTO users (id, github_id, github_username, tier) VALUES (?, ?, ?, 'Free')",
      )
      .bind("user_heal", 501, "user_heal")
      .run();
    await db
      .prepare(
        `INSERT INTO api_keys (user_id, key_hash, key_prefix, name, tier, limits_json)
         VALUES (?, ?, ?, 'Default', 'Free', '{"max_repos":1,"max_files":250,"max_loc":10000}')`,
      )
      .bind("user_heal", "hash_user_heal", "sk-recon-heal")
      .run();
    // Orphan placeholder: status='created', razorpay_subscription_id NULL.
    // Mirrors the post-/subscribe state where the UPDATE failed.
    const inserted = await db
      .prepare(
        `INSERT INTO subscriptions (user_id, tier, status)
         VALUES (?, 'Pro', 'created')
         RETURNING id`,
      )
      .bind("user_heal")
      .first<{ id: string }>();
    expect(inserted?.id).toBeTruthy();
    const placeholderId = inserted!.id;

    // Webhook arrives with notes.placeholder_id pointing at our orphan.
    const periodEnd = Math.floor(Date.now() / 1000) + 30 * 86_400;
    const body = JSON.stringify({
      event: "subscription.activated",
      created_at: Math.floor(Date.now() / 1000),
      payload: {
        subscription: {
          entity: {
            id: "sub_healed",
            plan_id: "plan_test",
            status: "active",
            current_start: Math.floor(Date.now() / 1000),
            current_end: periodEnd,
            notes: {
              user_id: "user_heal",
              tier: "Pro",
              currency: "USD",
              placeholder_id: placeholderId,
            },
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
      status: "ok",
      subscription_id: "sub_healed",
    });

    // The orphan row was stamped with the new sub_id and promoted to active.
    const subRow = await db
      .prepare(
        "SELECT id, razorpay_subscription_id, status, current_period_end FROM subscriptions WHERE user_id = ?",
      )
      .bind("user_heal")
      .first<{
        id: string;
        razorpay_subscription_id: string;
        status: string;
        current_period_end: string;
      }>();
    expect(subRow?.id).toBe(placeholderId);
    expect(subRow?.razorpay_subscription_id).toBe("sub_healed");
    expect(subRow?.status).toBe("active");
    expect(subRow?.current_period_end).toBe(
      new Date(periodEnd * 1000).toISOString(),
    );

    // Cascading user/api_keys updates ran.
    const userRow = await db
      .prepare("SELECT tier FROM users WHERE id = ?")
      .bind("user_heal")
      .first<{ tier: string }>();
    expect(userRow?.tier).toBe("Pro");
    const keyRow = await db
      .prepare("SELECT tier, expires_at FROM api_keys WHERE user_id = ?")
      .bind("user_heal")
      .first<{ tier: string; expires_at: string }>();
    expect(keyRow?.tier).toBe("Pro");
    expect(keyRow?.expires_at).toBe(new Date(periodEnd * 1000).toISOString());
  });

  it("null current_end: refuse to grant tier; record dropped event", async () => {
    // Bug: when Razorpay delivers a subscription event without `current_end`,
    // the old handler computes `periodEnd = null` and writes that NULL into
    // api_keys.expires_at. The hourly downgrade cron skips NULL rows by
    // design (it can't downgrade a never-expiring key), so the user gets a
    // permanent free Pro until manually corrected.
    //
    // Fix: refuse the event entirely — log it to webhook_events_dropped, do
    // not touch users/api_keys/subscriptions. The webhook still returns 2xx
    // so Razorpay doesn't retry-storm; we'll reconcile via portal/Razorpay
    // dashboard if needed.
    await seedSubscriber({
      userId: "user_nullend",
      githubId: 402,
      tier: "Pro",
      subId: "sub_nullend",
      subStatus: "created",
    });

    const body = subWebhook({
      event: "subscription.activated",
      subscriptionId: "sub_nullend",
      status: "active",
      // currentEnd intentionally omitted → subWebhook sends null
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

    // Subscription row: status NOT promoted to 'active'.
    const subRow = await db
      .prepare(
        "SELECT status, current_period_end FROM subscriptions WHERE razorpay_subscription_id = ?",
      )
      .bind("sub_nullend")
      .first<{ status: string; current_period_end: string | null }>();
    expect(subRow?.status).toBe("created");
    expect(subRow?.current_period_end).toBeNull();

    // users.tier stays Free.
    const userRow = await db
      .prepare("SELECT tier FROM users WHERE id = ?")
      .bind("user_nullend")
      .first<{ tier: string }>();
    expect(userRow?.tier).toBe("Free");

    // api_keys untouched: tier=Free, expires_at=null (CRUCIAL — a NULL
    // here under the bug would lock in permanent free Pro).
    const keyRow = await db
      .prepare(
        "SELECT tier, expires_at FROM api_keys WHERE user_id = ?",
      )
      .bind("user_nullend")
      .first<{ tier: string; expires_at: string | null }>();
    expect(keyRow?.tier).toBe("Free");
    expect(keyRow?.expires_at).toBeNull();

    // Audit trail: the dropped event must be recorded in webhook_events_dropped.
    const dropped = await db
      .prepare(
        "SELECT reason, event_type FROM webhook_events_dropped WHERE razorpay_subscription_id = ?",
      )
      .bind("sub_nullend")
      .first<{ reason: string; event_type: string }>();
    expect(dropped?.reason).toBe("missing_current_end");
    expect(dropped?.event_type).toBe("subscription.activated");
  });
});

describe("/v1/billing/subscribe — resume-or-swap on abandoned attempts", () => {
  beforeEach(async () => {
    await resetDb();
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  /** Same shape as the seedSession helper above but kept self-contained. */
  async function seed(userId: string, sessionToken: string): Promise<void> {
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const enc = new TextEncoder();
    const buf = await crypto.subtle.digest("SHA-256", enc.encode(sessionToken));
    const tokenHash = Array.from(new Uint8Array(buf))
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");
    const expiry = new Date(Date.now() + 86_400_000).toISOString();
    await db
      .prepare(
        "INSERT INTO users (id, github_id, github_username, tier) VALUES (?, ?, ?, 'Free')",
      )
      .bind(userId, Math.floor(Math.random() * 1_000_000_000), `user_${userId}`)
      .run();
    await db
      .prepare(
        "INSERT INTO sessions (user_id, token_hash, expires_at) VALUES (?, ?, ?)",
      )
      .bind(userId, tokenHash, expiry)
      .run();
  }

  /**
   * Stub global fetch so Razorpay calls return deterministic responses.
   * The closure tracks call counts per endpoint so tests can assert on
   * how many times each Razorpay API was hit. When `fetchSubReturns` is
   * provided, GET /subscriptions/:id returns that body; otherwise 404.
   */
  function stubRazorpay(opts: {
    fetchSubReturns?: { id: string; status: string; short_url: string };
    cancelOk?: boolean;
  }) {
    const counts = {
      createPlan: 0,
      createSubscription: 0,
      fetchSubscription: 0,
      cancel: 0,
    };
    const realFetch = globalThis.fetch;
    vi.stubGlobal(
      "fetch",
      async (
        input: RequestInfo | URL,
        init?: RequestInit,
      ): Promise<Response> => {
        const url =
          typeof input === "string"
            ? input
            : input instanceof URL
              ? input.toString()
              : input.url;
        if (!url.startsWith("https://api.razorpay.com")) {
          return realFetch(input, init);
        }
        const method = (init?.method ?? "GET").toUpperCase();
        if (url.endsWith("/plans") && method === "POST") {
          counts.createPlan++;
          return new Response(
            JSON.stringify({
              id: `plan_${counts.createPlan}`,
              entity: "plan",
              period: "monthly",
              interval: 1,
              item: { id: "i", name: "n", amount: 300, currency: "USD" },
            }),
            { status: 200, headers: { "Content-Type": "application/json" } },
          );
        }
        if (url.endsWith("/subscriptions") && method === "POST") {
          counts.createSubscription++;
          const body =
            typeof init?.body === "string" ? JSON.parse(init.body) : {};
          return new Response(
            JSON.stringify({
              id: `sub_new_${counts.createSubscription}`,
              entity: "subscription",
              plan_id: body.plan_id ?? "plan_x",
              status: "created",
              short_url: `https://rzp.io/i/new${counts.createSubscription}`,
              current_start: null,
              current_end: null,
              ended_at: null,
              quantity: 1,
              notes: body.notes ?? {},
              charge_at: null,
              start_at: null,
              end_at: null,
              auth_attempts: 0,
              total_count: 120,
              paid_count: 0,
              customer_notify: false,
              created_at: Math.floor(Date.now() / 1000),
              has_scheduled_changes: false,
              change_scheduled_at: null,
            }),
            { status: 200, headers: { "Content-Type": "application/json" } },
          );
        }
        // GET /subscriptions/<id> — fetchSubscription
        const subFetchMatch = url.match(/\/subscriptions\/([^\/]+)$/);
        if (subFetchMatch && method === "GET") {
          counts.fetchSubscription++;
          if (opts.fetchSubReturns) {
            return new Response(
              JSON.stringify({
                id: opts.fetchSubReturns.id,
                entity: "subscription",
                plan_id: "plan_existing",
                status: opts.fetchSubReturns.status,
                short_url: opts.fetchSubReturns.short_url,
                current_start: null,
                current_end: null,
                ended_at: null,
                quantity: 1,
                notes: {},
                charge_at: null,
                start_at: null,
                end_at: null,
                auth_attempts: 0,
                total_count: 120,
                paid_count: 0,
                customer_notify: false,
                created_at: Math.floor(Date.now() / 1000),
                has_scheduled_changes: false,
                change_scheduled_at: null,
              }),
              {
                status: 200,
                headers: { "Content-Type": "application/json" },
              },
            );
          }
          return new Response(
            JSON.stringify({
              error: { code: "BAD_REQUEST_ERROR", description: "not found" },
            }),
            { status: 404, headers: { "Content-Type": "application/json" } },
          );
        }
        // POST /subscriptions/<id>/cancel
        if (url.match(/\/subscriptions\/[^\/]+\/cancel$/) && method === "POST") {
          counts.cancel++;
          if (opts.cancelOk === false) {
            return new Response("upstream gone", { status: 400 });
          }
          return new Response(
            JSON.stringify({ status: "cancelled" }),
            { status: 200, headers: { "Content-Type": "application/json" } },
          );
        }
        return new Response("not stubbed", { status: 500 });
      },
    );
    return counts;
  }

  it("resumes the same Razorpay sub when the user retries the same tier+currency", async () => {
    // Opening the modal then dismissing leaves a 'created' placeholder.
    // Clicking Subscribe again with the same tier+currency must *not*
    // create a second Razorpay subscription — that would orphan the old
    // one upstream. Instead, the worker calls fetchSubscription on the
    // existing sub_id; when Razorpay confirms it's still 'created', the
    // worker returns the existing subscription_id with `resumed: true`.
    await seed("user_resume", "ses_resume");
    const counts = stubRazorpay({
      fetchSubReturns: {
        id: "sub_existing",
        status: "created",
        short_url: "https://rzp.io/i/existing",
      },
    });

    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    // Pre-seed an abandoned placeholder for Pro USD.
    await db
      .prepare(
        `INSERT INTO subscriptions
           (user_id, razorpay_subscription_id, tier, currency, status)
         VALUES (?, 'sub_existing', 'Pro', 'USD', 'created')`,
      )
      .bind("user_resume")
      .run();

    const res = await getJson("/v1/billing/subscribe", {
      method: "POST",
      headers: {
        Authorization: "Bearer ses_resume",
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ tier: "Pro", currency: "USD" }),
    });

    expect(res.status).toBe(200);
    expect(res.body).toMatchObject({
      subscription_id: "sub_existing",
      short_url: "https://rzp.io/i/existing",
      resumed: true,
      tier: "Pro",
    });

    // No new sub created upstream; only fetchSubscription was called.
    expect(counts.fetchSubscription).toBe(1);
    expect(counts.createSubscription).toBe(0);
    expect(counts.cancel).toBe(0);

    // D1 still has exactly one row, the original placeholder.
    const rows = await db
      .prepare(
        "SELECT razorpay_subscription_id, tier FROM subscriptions WHERE user_id = ?",
      )
      .bind("user_resume")
      .all<{ razorpay_subscription_id: string; tier: string }>();
    expect(rows.results.length).toBe(1);
    expect(rows.results[0].razorpay_subscription_id).toBe("sub_existing");
  });

  it("swaps to a fresh sub when the user retries with a different tier", async () => {
    // User clicked Subscribe to Pro, dismissed, then clicked Subscribe to
    // Team. The old Pro placeholder must be cancelled upstream and
    // deleted locally; a fresh Team subscription is created.
    await seed("user_swap", "ses_swap");
    const counts = stubRazorpay({
      // fetchSubscription is never reached on a tier mismatch — the
      // worker goes straight to swap.
    });

    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    await db
      .prepare(
        `INSERT INTO subscriptions
           (user_id, razorpay_subscription_id, tier, currency, status)
         VALUES (?, 'sub_old_pro', 'Pro', 'USD', 'created')`,
      )
      .bind("user_swap")
      .run();

    const res = await getJson("/v1/billing/subscribe", {
      method: "POST",
      headers: {
        Authorization: "Bearer ses_swap",
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ tier: "Team", currency: "USD" }),
    });

    expect(res.status).toBe(200);
    expect(res.body).toMatchObject({
      tier: "Team",
      subscription_id: "sub_new_1",
    });
    expect(res.body).not.toHaveProperty("resumed", true);

    // Razorpay was asked to cancel the old Pro sub once and create a
    // brand-new Team sub. fetchSubscription was NOT consulted (different
    // tier short-circuits the resume check).
    expect(counts.cancel).toBe(1);
    expect(counts.createSubscription).toBe(1);
    expect(counts.fetchSubscription).toBe(0);

    // D1 has only the new Team row; the old Pro placeholder was deleted.
    const rows = await db
      .prepare(
        "SELECT razorpay_subscription_id, tier FROM subscriptions WHERE user_id = ?",
      )
      .bind("user_swap")
      .all<{ razorpay_subscription_id: string; tier: string }>();
    expect(rows.results.length).toBe(1);
    expect(rows.results[0].tier).toBe("Team");
    expect(rows.results[0].razorpay_subscription_id).toBe("sub_new_1");
  });

  it("recreates from scratch when Razorpay 404s the stale placeholder", async () => {
    // Razorpay auto-expires unauthenticated subscriptions after their
    // internal TTL. The next /subscribe with the same tier+currency
    // should fetchSubscription, get a 404, fall through to swap, cancel
    // (best-effort, also fails — fine), delete D1 row, create fresh.
    await seed("user_stale", "ses_stale");
    const counts = stubRazorpay({
      // No fetchSubReturns → 404
      cancelOk: false,
    });

    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    await db
      .prepare(
        `INSERT INTO subscriptions
           (user_id, razorpay_subscription_id, tier, currency, status)
         VALUES (?, 'sub_expired', 'Pro', 'USD', 'created')`,
      )
      .bind("user_stale")
      .run();

    const res = await getJson("/v1/billing/subscribe", {
      method: "POST",
      headers: {
        Authorization: "Bearer ses_stale",
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ tier: "Pro", currency: "USD" }),
    });

    expect(res.status).toBe(200);
    expect(res.body).toMatchObject({
      tier: "Pro",
      subscription_id: "sub_new_1",
    });
    expect(res.body).not.toHaveProperty("resumed", true);

    // fetchSubscription was tried (and 404'd), upstream cancel was tried
    // (and failed — that's fine), then a fresh sub was created.
    expect(counts.fetchSubscription).toBe(1);
    expect(counts.cancel).toBe(1);
    expect(counts.createSubscription).toBe(1);

    const rows = await db
      .prepare(
        "SELECT razorpay_subscription_id FROM subscriptions WHERE user_id = ?",
      )
      .bind("user_stale")
      .all<{ razorpay_subscription_id: string }>();
    expect(rows.results.length).toBe(1);
    expect(rows.results[0].razorpay_subscription_id).toBe("sub_new_1");
  });

  it("/v1/billing/cancel cancels a 'created' placeholder immediately and unblocks resubscribe", async () => {
    // The dashboard's Cancel button now renders for 'created' placeholders
    // (the user clicked Subscribe but dismissed the modal). Hitting
    // /cancel should:
    //   - call Razorpay cancel with cancel_at_cycle_end=false
    //   - flip the local row to 'cancelled' immediately (no period to honor)
    //   - allow the user to /subscribe again right away
    await seed("user_cancel_pl", "ses_cancel_pl");
    const counts = stubRazorpay({});

    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    await db
      .prepare(
        `INSERT INTO subscriptions
           (user_id, razorpay_subscription_id, tier, currency, status)
         VALUES (?, 'sub_to_cancel', 'Pro', 'USD', 'created')`,
      )
      .bind("user_cancel_pl")
      .run();

    const cancelRes = await getJson("/v1/billing/cancel", {
      method: "POST",
      headers: {
        Authorization: "Bearer ses_cancel_pl",
        "Content-Type": "application/json",
      },
    });
    expect(cancelRes.status).toBe(200);
    expect(cancelRes.body).toMatchObject({
      status: "cancelled",
      tier: "Pro",
    });
    expect(counts.cancel).toBe(1);

    // Local row immediately flipped to 'cancelled'.
    const row = await db
      .prepare(
        "SELECT status, cancelled_at FROM subscriptions WHERE razorpay_subscription_id = ?",
      )
      .bind("sub_to_cancel")
      .first<{ status: string; cancelled_at: string }>();
    expect(row?.status).toBe("cancelled");
    expect(row?.cancelled_at).not.toBeNull();

    // User can now /subscribe again — no 409 from leftover placeholder.
    const newRes = await getJson("/v1/billing/subscribe", {
      method: "POST",
      headers: {
        Authorization: "Bearer ses_cancel_pl",
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ tier: "Pro", currency: "USD" }),
    });
    expect(newRes.status).toBe(200);
    expect(newRes.body).toMatchObject({
      tier: "Pro",
      subscription_id: "sub_new_1",
    });
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
