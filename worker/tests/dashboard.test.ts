/**
 * Dashboard route tests.
 *
 * Focus: the key lifecycle (list → create → delete). Unauth-ed callers
 * must 401 before any DB work; revoke must hard-delete so the row
 * disappears from the next /keys list. Ownership boundaries matter:
 * user A must not be able to delete user B's key even by guessing the id.
 */

import { beforeEach, describe, expect, it } from "vitest";
import { env, getJson, resetDb } from "./setup";

/** SHA-256 hex — matches sha256Hex in the Worker. */
async function sha256Hex(input: string): Promise<string> {
  const buf = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(input),
  );
  return Array.from(new Uint8Array(buf))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/**
 * Seed a user + session + one API key. Returns the session token so
 * tests can authenticate via `Authorization: Bearer …`.
 * (The cookie path is tested on the production alias; in this unit
 * suite we use the Bearer fallback the middleware also accepts.)
 */
async function seedUserWithKey(opts: {
  userId: string;
  username: string;
  keyId: string;
  keyValue: string;
}): Promise<{ sessionToken: string }> {
  const db = (env as { RECON_DB: D1Database }).RECON_DB;
  const sessionToken = "test-session-" + opts.userId;
  const tokenHash = await sha256Hex(sessionToken);
  const expiresAt = new Date(Date.now() + 86_400_000).toISOString();
  const keyHash = await sha256Hex(opts.keyValue);

  await db
    .prepare(
      "INSERT INTO users (id, github_id, github_username, tier) VALUES (?, ?, ?, 'Pro')",
    )
    .bind(opts.userId, Math.floor(Math.random() * 1_000_000), opts.username)
    .run();
  await db
    .prepare(
      "INSERT INTO sessions (user_id, token_hash, expires_at) VALUES (?, ?, ?)",
    )
    .bind(opts.userId, tokenHash, expiresAt)
    .run();
  await db
    .prepare(
      `INSERT INTO api_keys (id, user_id, key_hash, key_prefix, name, tier, limits_json)
       VALUES (?, ?, ?, ?, 'Default', 'Pro', '{"max_repos":10,"max_files":50000,"max_loc":2000000}')`,
    )
    .bind(opts.keyId, opts.userId, keyHash, opts.keyValue.slice(0, 14))
    .run();
  return { sessionToken };
}

describe("DELETE /v1/dashboard/keys/:id", () => {
  beforeEach(async () => {
    await resetDb();
  });

  it("401 when the caller is unauthenticated", async () => {
    const res = await getJson("/v1/dashboard/keys/some-id", {
      method: "DELETE",
    });
    expect(res.status).toBe(401);
  });

  it("hard-deletes the key and it disappears from the next /keys list", async () => {
    const { sessionToken } = await seedUserWithKey({
      userId: "user_alice",
      username: "alice",
      keyId: "key_alice_1",
      keyValue: "sk-recon-alicekey1",
    });
    const authHeaders = { Authorization: `Bearer ${sessionToken}` };

    // Before: one key.
    const before = await getJson("/v1/dashboard/keys", { headers: authHeaders });
    expect(before.status).toBe(200);
    type KeysBody = { keys: { id: string }[] };
    expect((before.body as KeysBody).keys.length).toBe(1);

    // Revoke → hard delete.
    const del = await getJson("/v1/dashboard/keys/key_alice_1", {
      method: "DELETE",
      headers: authHeaders,
    });
    expect(del.status).toBe(200);
    expect(del.body).toMatchObject({ ok: true });

    // After: zero keys — the row is gone, not just marked.
    const after = await getJson("/v1/dashboard/keys", { headers: authHeaders });
    expect((after.body as KeysBody).keys.length).toBe(0);

    // Row removed at the DB layer too.
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const row = await db
      .prepare("SELECT id FROM api_keys WHERE id = ?")
      .bind("key_alice_1")
      .first();
    expect(row).toBeNull();
  });

  it("404 on unknown id without leaking existence", async () => {
    const { sessionToken } = await seedUserWithKey({
      userId: "user_bob",
      username: "bob",
      keyId: "key_bob_1",
      keyValue: "sk-recon-bobkey1234",
    });
    const res = await getJson("/v1/dashboard/keys/does-not-exist", {
      method: "DELETE",
      headers: { Authorization: `Bearer ${sessionToken}` },
    });
    expect(res.status).toBe(404);
  });

  it("404 when user A tries to delete user B's key (ownership boundary)", async () => {
    // Alice is the attacker; carol owns the key. Alice guesses carol's
    // key id and tries to delete it with her own session.
    const { sessionToken: aliceToken } = await seedUserWithKey({
      userId: "user_alice2",
      username: "alice2",
      keyId: "key_alice_2",
      keyValue: "sk-recon-alice2key",
    });
    await seedUserWithKey({
      userId: "user_carol",
      username: "carol",
      keyId: "key_carol_1",
      keyValue: "sk-recon-carolkey1",
    });

    const res = await getJson("/v1/dashboard/keys/key_carol_1", {
      method: "DELETE",
      headers: { Authorization: `Bearer ${aliceToken}` },
    });
    // Same 404 code as "truly not found" — do not leak that carol's key
    // exists via a different status or error message.
    expect(res.status).toBe(404);

    // Carol's key must still be in the DB.
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const row = await db
      .prepare("SELECT id FROM api_keys WHERE id = ?")
      .bind("key_carol_1")
      .first();
    expect(row).not.toBeNull();
  });
});

describe("POST /v1/dashboard/keys — expires_at inheritance from active subscription", () => {
  beforeEach(async () => {
    await resetDb();
  });

  /**
   * Seed user + session only (no api_key yet). Returns the session so a
   * POST /keys test can authenticate and create a key from scratch.
   */
  async function seedUserSession(opts: {
    userId: string;
    username: string;
    tier: string;
  }): Promise<{ sessionToken: string }> {
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const sessionToken = "test-session-" + opts.userId;
    const tokenHash = await sha256Hex(sessionToken);
    const expiresAt = new Date(Date.now() + 86_400_000).toISOString();

    await db
      .prepare(
        "INSERT INTO users (id, github_id, github_username, tier) VALUES (?, ?, ?, ?)",
      )
      .bind(opts.userId, Math.floor(Math.random() * 1_000_000), opts.username, opts.tier)
      .run();
    await db
      .prepare(
        "INSERT INTO sessions (user_id, token_hash, expires_at) VALUES (?, ?, ?)",
      )
      .bind(opts.userId, tokenHash, expiresAt)
      .run();
    return { sessionToken };
  }

  it("blocks generating a second key while one is active (409)", async () => {
    // Seed user with an existing non-revoked key, then attempt to generate
    // a second one. Worker must return 409 with the existing-key metadata
    // so the UI can tell the user to rotate instead.
    const { sessionToken } = await seedUserWithKey({
      userId: "user_stacking",
      username: "stacker",
      keyId: "key_stacker_1",
      keyValue: "sk-recon-stacker1key",
    });

    const res = await getJson("/v1/dashboard/keys", {
      method: "POST",
      headers: {
        Authorization: `Bearer ${sessionToken}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ name: "second" }),
    });
    expect(res.status).toBe(409);
    expect(res.body).toMatchObject({
      existing_key_id: "key_stacker_1",
    });

    // The existing key is still the only row — no silent stacking.
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const count = await db
      .prepare(
        "SELECT COUNT(*) as n FROM api_keys WHERE user_id = ? AND revoked_at IS NULL",
      )
      .bind("user_stacking")
      .first<{ n: number }>();
    expect(count?.n).toBe(1);
  });

  it("allows generating a new key after the previous one is revoked (rotation)", async () => {
    // Rotation path: DELETE the existing key, then POST to get a fresh one.
    // The UI's "Rotate key" button runs these two in sequence.
    const { sessionToken } = await seedUserWithKey({
      userId: "user_rotate",
      username: "rotator",
      keyId: "key_rot_1",
      keyValue: "sk-recon-rotate1key",
    });
    const headers = {
      Authorization: `Bearer ${sessionToken}`,
      "Content-Type": "application/json",
    };

    // First generate attempt → 409 (existing key).
    const blocked = await getJson("/v1/dashboard/keys", {
      method: "POST",
      headers,
      body: JSON.stringify({ name: "Default" }),
    });
    expect(blocked.status).toBe(409);

    // Revoke.
    const del = await getJson("/v1/dashboard/keys/key_rot_1", {
      method: "DELETE",
      headers,
    });
    expect(del.status).toBe(200);

    // Second generate → success.
    const fresh = await getJson("/v1/dashboard/keys", {
      method: "POST",
      headers,
      body: JSON.stringify({ name: "Default" }),
    });
    expect(fresh.status).toBe(200);
    expect(fresh.body).toMatchObject({ tier: "Pro" });
  });

  it("Free user with no subscription gets a key with expires_at = null", async () => {
    const { sessionToken } = await seedUserSession({
      userId: "user_free_new",
      username: "freshfree",
      tier: "Free",
    });
    const res = await getJson("/v1/dashboard/keys", {
      method: "POST",
      headers: {
        Authorization: `Bearer ${sessionToken}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ name: "Default" }),
    });
    expect(res.status).toBe(200);
    type CreateBody = { key: string; tier: string; expires_at: string | null };
    const body = res.body as CreateBody;
    expect(body.tier).toBe("Free");
    expect(body.expires_at).toBeNull();

    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const row = await db
      .prepare("SELECT expires_at FROM api_keys WHERE user_id = ?")
      .bind("user_free_new")
      .first<{ expires_at: string | null }>();
    expect(row?.expires_at).toBeNull();
  });

  it("Pro user with active subscription inherits subscription.current_period_end on new key", async () => {
    // The real bug the test replays: a user revokes their existing key mid-
    // subscription, generates a new one. Before the fix, the new key had
    // expires_at = NULL and the downgrade cron's WHERE clause never matched
    // it — the key stayed Pro forever even after the sub ended.
    const { sessionToken } = await seedUserSession({
      userId: "user_pro_active",
      username: "pro_active",
      tier: "Pro",
    });
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const periodEnd = new Date(Date.now() + 30 * 86_400_000).toISOString();
    await db
      .prepare(
        `INSERT INTO subscriptions
           (user_id, razorpay_subscription_id, tier, status, current_period_end)
         VALUES (?, 'sub_active', 'Pro', 'active', ?)`,
      )
      .bind("user_pro_active", periodEnd)
      .run();

    const res = await getJson("/v1/dashboard/keys", {
      method: "POST",
      headers: {
        Authorization: `Bearer ${sessionToken}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ name: "Default" }),
    });
    expect(res.status).toBe(200);
    type CreateBody = { tier: string; expires_at: string | null };
    const body = res.body as CreateBody;
    expect(body.tier).toBe("Pro");
    expect(body.expires_at).toBe(periodEnd);

    const row = await db
      .prepare("SELECT expires_at FROM api_keys WHERE user_id = ?")
      .bind("user_pro_active")
      .first<{ expires_at: string | null }>();
    expect(row?.expires_at).toBe(periodEnd);
  });

  it("cancelled-at-period-end subscription still stamps expires_at — cron must downgrade after period passes", async () => {
    const { sessionToken } = await seedUserSession({
      userId: "user_pro_cancelled",
      username: "pro_cancelled",
      tier: "Pro",
    });
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const periodEnd = new Date(Date.now() + 10 * 86_400_000).toISOString();
    await db
      .prepare(
        `INSERT INTO subscriptions
           (user_id, razorpay_subscription_id, tier, status,
            current_period_end, cancel_at_period_end, cancelled_at)
         VALUES (?, 'sub_cancelled', 'Pro', 'active', ?, 1, datetime('now'))`,
      )
      .bind("user_pro_cancelled", periodEnd)
      .run();

    const res = await getJson("/v1/dashboard/keys", {
      method: "POST",
      headers: {
        Authorization: `Bearer ${sessionToken}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ name: "Default" }),
    });
    const body = res.body as { expires_at: string };
    // The key must still expire at period_end — that's the whole point of
    // honor-until-period-end. Missing expires_at here is the bug.
    expect(body.expires_at).toBe(periodEnd);
  });

  it("stale 'expired' subscription does NOT leak period_end onto a fresh key", async () => {
    // A user whose previous Pro sub ended long ago and has been downgraded
    // to Free. They generate a new key: it should be Free with expires_at
    // NULL. Pulling the old sub's period_end would re-enable a now-deleted
    // grace window.
    const { sessionToken } = await seedUserSession({
      userId: "user_expired",
      username: "expired_user",
      tier: "Free",
    });
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const pastEnd = new Date(Date.now() - 30 * 86_400_000).toISOString();
    await db
      .prepare(
        `INSERT INTO subscriptions
           (user_id, razorpay_subscription_id, tier, status, current_period_end)
         VALUES (?, 'sub_expired', 'Pro', 'completed', ?)`,
      )
      .bind("user_expired", pastEnd)
      .run();

    const res = await getJson("/v1/dashboard/keys", {
      method: "POST",
      headers: {
        Authorization: `Bearer ${sessionToken}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ name: "Default" }),
    });
    const body = res.body as { tier: string; expires_at: string | null };
    expect(body.tier).toBe("Free");
    expect(body.expires_at).toBeNull();
  });
});

describe("DELETE /v1/dashboard/account — cascade delete all user data", () => {
  beforeEach(async () => {
    await resetDb();
  });

  it("401 when unauthenticated", async () => {
    const res = await getJson("/v1/dashboard/account", { method: "DELETE" });
    expect(res.status).toBe(401);
  });

  it("deletes user + cascades to api_keys, sessions, payments, subscriptions, payment_events", async () => {
    // Seed a user with the whole messy state a real account accumulates.
    const { sessionToken } = await seedUserWithKey({
      userId: "user_doomed",
      username: "doomed",
      keyId: "key_doomed",
      keyValue: "sk-recon-doomedkey1",
    });
    const db = (env as { RECON_DB: D1Database }).RECON_DB;

    // A payment (with no Razorpay sub — legacy one-time)
    await db
      .prepare(
        `INSERT INTO payments (user_id, razorpay_order_id, amount_paise, currency, status, tier)
         VALUES (?, 'order_doomed', 300, 'USD', 'captured', 'Pro')`,
      )
      .bind("user_doomed")
      .run();

    // A payment_event tied to that order_id (no FK — we clean it manually)
    await db
      .prepare(
        `INSERT INTO payment_events (razorpay_payment_id, event_type, razorpay_order_id, processed_at)
         VALUES ('pay_doomed', 'payment.captured', 'order_doomed', datetime('now'))`,
      )
      .run();

    // A subscription with NULL razorpay_subscription_id (never completed
    // Razorpay creation) — the delete handler must skip the Razorpay
    // cancel call for these rather than crashing on a missing ID.
    await db
      .prepare(
        `INSERT INTO subscriptions (user_id, tier, status, razorpay_subscription_id)
         VALUES (?, 'Pro', 'created', NULL)`,
      )
      .bind("user_doomed")
      .run();

    const res = await getJson("/v1/dashboard/account", {
      method: "DELETE",
      headers: { Authorization: `Bearer ${sessionToken}` },
    });
    expect(res.status).toBe(200);
    expect(res.body).toMatchObject({ ok: true, user_id: "user_doomed" });

    // Every row referencing this user should be gone.
    for (const [table, col] of [
      ["users", "id"],
      ["api_keys", "user_id"],
      ["sessions", "user_id"],
      ["payments", "user_id"],
      ["subscriptions", "user_id"],
    ] as const) {
      const row = await db
        .prepare(`SELECT COUNT(*) as n FROM ${table} WHERE ${col} = ?`)
        .bind("user_doomed")
        .first<{ n: number }>();
      expect(row?.n).toBe(0);
    }

    // payment_events cleaned by order_id join — no orphan idempotency rows.
    const eventRow = await db
      .prepare(
        "SELECT COUNT(*) as n FROM payment_events WHERE razorpay_order_id = ?",
      )
      .bind("order_doomed")
      .first<{ n: number }>();
    expect(eventRow?.n).toBe(0);
  });

  it("does not touch other users' rows", async () => {
    const { sessionToken: aliceToken } = await seedUserWithKey({
      userId: "user_delete_me",
      username: "alice_delete",
      keyId: "key_alice_delete",
      keyValue: "sk-recon-alicedelete",
    });
    await seedUserWithKey({
      userId: "user_keep_me",
      username: "bob_keep",
      keyId: "key_bob_keep",
      keyValue: "sk-recon-bobkeepkey",
    });

    const res = await getJson("/v1/dashboard/account", {
      method: "DELETE",
      headers: { Authorization: `Bearer ${aliceToken}` },
    });
    expect(res.status).toBe(200);

    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const bob = await db
      .prepare("SELECT id FROM users WHERE id = ?")
      .bind("user_keep_me")
      .first();
    expect(bob).not.toBeNull();
    const bobKey = await db
      .prepare("SELECT id FROM api_keys WHERE id = ?")
      .bind("key_bob_keep")
      .first();
    expect(bobKey).not.toBeNull();
  });
});

// ─── /v1/dashboard/repos ───────────────────────────────────────────────────────
//
// Session-cookie endpoint that mirrors /v1/account/repos (which is API-key
// Bearer). The dashboard JS uses these so users with revoked-by-quota repos
// can manage slots without dropping to the CLI.

describe("GET /v1/dashboard/repos", () => {
  beforeEach(async () => {
    await resetDb();
  });

  it("401 when unauthenticated", async () => {
    const res = await getJson("/v1/dashboard/repos");
    expect(res.status).toBe(401);
  });

  it("returns empty list + tier limits for a fresh account", async () => {
    const { sessionToken } = await seedUserWithKey({
      userId: "user_repos_empty",
      username: "ralph",
      keyId: "key_repos_empty",
      keyValue: "sk-recon-emptyrepos1",
    });
    const res = await getJson("/v1/dashboard/repos", {
      headers: { Authorization: `Bearer ${sessionToken}` },
    });
    expect(res.status).toBe(200);
    expect(res.body).toMatchObject({ repos: [], tier: "Pro", limit: 10 });
  });

  it("returns the user's repos sorted by last_seen desc", async () => {
    const { sessionToken } = await seedUserWithKey({
      userId: "user_repos_list",
      username: "lila",
      keyId: "key_repos_list",
      keyValue: "sk-recon-listrepos1",
    });
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    // Seed three repos with deterministic last_seen so we can assert order.
    await db
      .prepare(
        `INSERT INTO user_repos (user_id, fingerprint, first_seen_at, last_seen_at)
         VALUES (?, ?, '2026-04-01T00:00:00', '2026-04-10T00:00:00'),
                (?, ?, '2026-04-02T00:00:00', '2026-04-25T00:00:00'),
                (?, ?, '2026-04-03T00:00:00', '2026-04-15T00:00:00')`,
      )
      .bind(
        "user_repos_list", "a".repeat(64),
        "user_repos_list", "b".repeat(64),
        "user_repos_list", "c".repeat(64),
      )
      .run();

    const res = await getJson("/v1/dashboard/repos", {
      headers: { Authorization: `Bearer ${sessionToken}` },
    });
    expect(res.status).toBe(200);
    type Body = {
      repos: Array<{ fingerprint: string; last_seen_at: string }>;
      limit: number;
      tier: string;
    };
    const body = res.body as Body;
    expect(body.repos.length).toBe(3);
    expect(body.repos[0].fingerprint).toBe("b".repeat(64)); // newest last_seen
    expect(body.repos[2].fingerprint).toBe("a".repeat(64)); // oldest last_seen
    expect(body.limit).toBe(10);
    expect(body.tier).toBe("Pro");
  });

  it("does not leak repos from other users", async () => {
    const { sessionToken: aliceToken } = await seedUserWithKey({
      userId: "user_repos_a",
      username: "alice_repos",
      keyId: "key_repos_a",
      keyValue: "sk-recon-aliceisolation1",
    });
    await seedUserWithKey({
      userId: "user_repos_b",
      username: "bob_repos",
      keyId: "key_repos_b",
      keyValue: "sk-recon-bobisolation1",
    });
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    await db
      .prepare(
        `INSERT INTO user_repos (user_id, fingerprint) VALUES (?, ?), (?, ?)`,
      )
      .bind(
        "user_repos_a", "a".repeat(64),
        "user_repos_b", "b".repeat(64),
      )
      .run();

    const res = await getJson("/v1/dashboard/repos", {
      headers: { Authorization: `Bearer ${aliceToken}` },
    });
    type Body = { repos: Array<{ fingerprint: string }> };
    const body = res.body as Body;
    expect(body.repos.length).toBe(1);
    expect(body.repos[0].fingerprint).toBe("a".repeat(64));
  });
});

describe("DELETE /v1/dashboard/repos/:fingerprint", () => {
  beforeEach(async () => {
    await resetDb();
  });

  it("401 when unauthenticated", async () => {
    const res = await getJson(`/v1/dashboard/repos/${"0".repeat(64)}`, {
      method: "DELETE",
    });
    expect(res.status).toBe(401);
  });

  it("400 on a malformed fingerprint", async () => {
    const { sessionToken } = await seedUserWithKey({
      userId: "user_repo_bad_fp",
      username: "bad_fp",
      keyId: "key_repo_bad_fp",
      keyValue: "sk-recon-badfp1",
    });
    const res = await getJson("/v1/dashboard/repos/not-hex", {
      method: "DELETE",
      headers: { Authorization: `Bearer ${sessionToken}` },
    });
    expect(res.status).toBe(400);
  });

  it("404 when the fingerprint is not in the user's set", async () => {
    const { sessionToken } = await seedUserWithKey({
      userId: "user_repo_404",
      username: "fournotfour",
      keyId: "key_repo_404",
      keyValue: "sk-recon-404repos1",
    });
    const res = await getJson(`/v1/dashboard/repos/${"d".repeat(64)}`, {
      method: "DELETE",
      headers: { Authorization: `Bearer ${sessionToken}` },
    });
    expect(res.status).toBe(404);
  });

  it("204 + frees the slot for the next register", async () => {
    const { sessionToken } = await seedUserWithKey({
      userId: "user_repo_del",
      username: "delly",
      keyId: "key_repo_del",
      keyValue: "sk-recon-deletekeep1",
    });
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    await db
      .prepare("INSERT INTO user_repos (user_id, fingerprint) VALUES (?, ?)")
      .bind("user_repo_del", "f".repeat(64))
      .run();

    const res = await getJson(`/v1/dashboard/repos/${"f".repeat(64)}`, {
      method: "DELETE",
      headers: { Authorization: `Bearer ${sessionToken}` },
    });
    expect(res.status).toBe(204);

    const row = await db
      .prepare("SELECT 1 AS one FROM user_repos WHERE user_id = ? AND fingerprint = ?")
      .bind("user_repo_del", "f".repeat(64))
      .first();
    expect(row).toBeNull();
  });

  it("user A cannot delete user B's fingerprint", async () => {
    const { sessionToken: aliceToken } = await seedUserWithKey({
      userId: "user_repo_xa",
      username: "alice_xrepo",
      keyId: "key_repo_xa",
      keyValue: "sk-recon-xrepoalice1",
    });
    await seedUserWithKey({
      userId: "user_repo_xb",
      username: "bob_xrepo",
      keyId: "key_repo_xb",
      keyValue: "sk-recon-xrepobob1",
    });
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    await db
      .prepare("INSERT INTO user_repos (user_id, fingerprint) VALUES (?, ?)")
      .bind("user_repo_xb", "9".repeat(64))
      .run();

    const res = await getJson(`/v1/dashboard/repos/${"9".repeat(64)}`, {
      method: "DELETE",
      headers: { Authorization: `Bearer ${aliceToken}` },
    });
    expect(res.status).toBe(404);

    // Bob's row must still be present.
    const row = await db
      .prepare("SELECT 1 AS one FROM user_repos WHERE user_id = ? AND fingerprint = ?")
      .bind("user_repo_xb", "9".repeat(64))
      .first();
    expect(row).not.toBeNull();
  });
});
