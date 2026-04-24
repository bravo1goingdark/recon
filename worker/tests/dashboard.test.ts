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
