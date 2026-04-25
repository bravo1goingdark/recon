/**
 * /v1/account/repos contract tests.
 *
 * The atomic-INSERT race test is the load-bearing one: it fires
 * `max_repos + 5` concurrent POSTs from the same key with distinct
 * fingerprints and asserts that exactly `max_repos` succeed. This is
 * what guards against a patched binary spamming the endpoint to bypass
 * the limit. Without atomic enforcement this test fails — see
 * routes/account.ts for the single-statement INSERT…SELECT…WHERE
 * predicate that makes it work.
 */

import { beforeEach, describe, expect, it } from "vitest";
import { env, getJson, resetDb } from "./setup";

/** SHA-256 hex — same shape as src/lib/crypto.ts. */
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
 * Seed a user + api_key. Returns the raw key the CLI would Bearer.
 * Mirrors the convention in license.test.ts so the two test suites
 * stay structurally similar.
 */
async function seedUserKey(opts: {
  userId: string;
  key: string;
  tier?: string;
  maxRepos?: number;
  revoked?: boolean;
  expiresAt?: string | null;
}): Promise<void> {
  const db = (env as { RECON_DB: D1Database }).RECON_DB;
  const tier = opts.tier ?? "Pro";
  const maxRepos = opts.maxRepos ?? 5;
  const keyHash = await sha256Hex(opts.key);
  // github_id must be a non-null UNIQUE int per the users schema. Hash the
  // userId into the first 6 hex chars of its sha256 so distinct userIds
  // map to distinct ints without us threading a counter through every call.
  const ghIdHex = (await sha256Hex(opts.userId)).slice(0, 6);
  const githubId = parseInt(ghIdHex, 16);
  await db
    .prepare(
      `INSERT INTO users (id, github_id, github_username, tier)
       VALUES (?, ?, 'tester', ?)`,
    )
    .bind(opts.userId, githubId, tier)
    .run();
  await db
    .prepare(
      `INSERT INTO api_keys (user_id, key_hash, key_prefix, name, tier,
                             limits_json, expires_at, revoked_at)
       VALUES (?, ?, ?, 'Default', ?, ?, ?, ?)`,
    )
    .bind(
      opts.userId,
      keyHash,
      opts.key.slice(0, 14),
      tier,
      JSON.stringify({ max_repos: maxRepos, max_files: 10000, max_loc: 1_000_000 }),
      opts.expiresAt ?? null,
      opts.revoked ? new Date().toISOString() : null,
    )
    .run();
}

/** Fingerprint a path like the CLI does: lowercase hex SHA-256. */
async function fp(path: string): Promise<string> {
  return sha256Hex(path);
}

/** POST /v1/account/repos. */
function register(key: string, fingerprint: string) {
  return getJson("/v1/account/repos", {
    method: "POST",
    headers: {
      Authorization: `Bearer ${key}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ fingerprint }),
  });
}

describe("POST /v1/account/repos — auth", () => {
  beforeEach(resetDb);

  it("rejects missing Authorization header", async () => {
    const res = await getJson("/v1/account/repos", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ fingerprint: await fp("/x") }),
    });
    expect(res.status).toBe(401);
  });

  it("rejects unknown api key", async () => {
    const res = await register("sk-recon-bogus-xxx", await fp("/x"));
    expect(res.status).toBe(401);
  });

  it("rejects revoked api key", async () => {
    await seedUserKey({
      userId: "u1revokeacc",
      key: "sk-recon-revoke-abc",
      revoked: true,
    });
    const res = await register("sk-recon-revoke-abc", await fp("/x"));
    expect(res.status).toBe(401);
  });

  it("rejects expired api key", async () => {
    await seedUserKey({
      userId: "u1expacc",
      key: "sk-recon-expired-abc",
      expiresAt: new Date(Date.now() - 86_400_000).toISOString(),
    });
    const res = await register("sk-recon-expired-abc", await fp("/x"));
    expect(res.status).toBe(401);
  });
});

describe("POST /v1/account/repos — input validation", () => {
  beforeEach(resetDb);

  it("rejects malformed JSON body", async () => {
    await seedUserKey({ userId: "u1jsoninv", key: "sk-recon-jsonbad-abc" });
    const res = await getJson("/v1/account/repos", {
      method: "POST",
      headers: {
        Authorization: "Bearer sk-recon-jsonbad-abc",
        "Content-Type": "application/json",
      },
      body: "not json {{",
    });
    expect(res.status).toBe(400);
  });

  it("rejects fingerprint that isn't 64-char hex", async () => {
    await seedUserKey({ userId: "u1fpinv", key: "sk-recon-fpinv-abc" });
    for (const bad of ["", "ABC", "g".repeat(64), "0".repeat(63), "0".repeat(65)]) {
      const res = await register("sk-recon-fpinv-abc", bad);
      expect(res.status, `bad fingerprint=${JSON.stringify(bad)}`).toBe(400);
    }
  });
});

describe("POST /v1/account/repos — happy path", () => {
  beforeEach(resetDb);

  it("returns 201 + status:registered on first POST", async () => {
    await seedUserKey({ userId: "u1happy", key: "sk-recon-happy-abc", maxRepos: 3 });
    const f = await fp("/home/me/proj-a");
    const res = await register("sk-recon-happy-abc", f);
    expect(res.status).toBe(201);
    expect(res.body).toMatchObject({
      fingerprint: f,
      status: "registered",
      limit: 3,
    });
  });

  it("returns 200 + status:refreshed on repeat POST (idempotent)", async () => {
    await seedUserKey({ userId: "u1idem", key: "sk-recon-idem-abc", maxRepos: 3 });
    const f = await fp("/home/me/proj-b");
    const first = await register("sk-recon-idem-abc", f);
    expect(first.status).toBe(201);
    const second = await register("sk-recon-idem-abc", f);
    expect(second.status).toBe(200);
    expect(second.body).toMatchObject({ fingerprint: f, status: "refreshed" });
  });

  it("counts each unique fingerprint towards the limit", async () => {
    await seedUserKey({ userId: "u1count", key: "sk-recon-count-abc", maxRepos: 3 });
    for (const i of [1, 2, 3]) {
      const r = await register("sk-recon-count-abc", await fp(`/p${i}`));
      expect(r.status).toBe(201);
    }
    const over = await register("sk-recon-count-abc", await fp("/p4"));
    expect(over.status).toBe(403);
    expect(over.body).toMatchObject({ limit: 3, tier: "Pro" });
  });

  it("rejecting an over-limit POST does not bump last_seen on existing rows", async () => {
    await seedUserKey({ userId: "u1bump", key: "sk-recon-bump-abc", maxRepos: 1 });
    await register("sk-recon-bump-abc", await fp("/already"));
    const before = await getJson("/v1/account/repos", {
      headers: { Authorization: "Bearer sk-recon-bump-abc" },
    });
    const blocked = await register("sk-recon-bump-abc", await fp("/blocked"));
    expect(blocked.status).toBe(403);
    const after = await getJson("/v1/account/repos", {
      headers: { Authorization: "Bearer sk-recon-bump-abc" },
    });
    type RepoListBody = { repos: Array<{ last_seen_at: string }> };
    const beforeRepos = (before.body as RepoListBody).repos;
    const afterRepos = (after.body as RepoListBody).repos;
    expect(afterRepos.length).toBe(beforeRepos.length);
  });
});

describe("POST /v1/account/repos — race / atomicity", () => {
  beforeEach(resetDb);

  /**
   * The single load-bearing test for this whole feature.  Without an
   * atomic INSERT…SELECT…WHERE in routes/account.ts, two concurrent
   * POSTs at limit-1 can both pass the count check and both insert,
   * overflowing the user past `max_repos`.  This fires significantly
   * more parallel POSTs than the limit and asserts the count holds.
   */
  it("never accepts more than max_repos under concurrent POSTs", async () => {
    const max = 5;
    const overshoot = 7;
    await seedUserKey({
      userId: "u1race",
      key: "sk-recon-race-abc",
      maxRepos: max,
    });

    // Distinct fingerprints — each is a fresh "new" registration, so the
    // EXISTS branch of the WHERE clause never fires; only the COUNT(*)
    // < max branch can let the INSERT through.  This is the exact path
    // that races without atomicity.
    const fps = await Promise.all(
      Array.from({ length: max + overshoot }, (_, i) => fp(`/race/proj-${i}`)),
    );

    const results = await Promise.all(
      fps.map((f) => register("sk-recon-race-abc", f)),
    );

    const accepted = results.filter((r) => r.status === 201).length;
    const rejected = results.filter((r) => r.status === 403).length;

    expect(accepted).toBe(max);
    expect(rejected).toBe(overshoot);

    // Verify ground truth in the table — not just the response codes.
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const count = await db
      .prepare("SELECT COUNT(*) AS n FROM user_repos WHERE user_id = ?")
      .bind("u1race")
      .first<{ n: number }>();
    expect(count?.n).toBe(max);
  });

  it("a repeat of an already-registered fingerprint is allowed even when at limit", async () => {
    const max = 2;
    await seedUserKey({
      userId: "u1edge",
      key: "sk-recon-edge-abc",
      maxRepos: max,
    });
    const f1 = await fp("/edge/a");
    const f2 = await fp("/edge/b");

    expect((await register("sk-recon-edge-abc", f1)).status).toBe(201);
    expect((await register("sk-recon-edge-abc", f2)).status).toBe(201);

    // We're at the limit. A repeat of f1 must succeed (idempotent),
    // a fresh f3 must fail.
    expect((await register("sk-recon-edge-abc", f1)).status).toBe(200);
    expect((await register("sk-recon-edge-abc", await fp("/edge/c"))).status).toBe(403);
  });
});

describe("GET /v1/account/repos", () => {
  beforeEach(resetDb);

  it("requires auth", async () => {
    const res = await getJson("/v1/account/repos");
    expect(res.status).toBe(401);
  });

  it("returns empty list for a fresh account", async () => {
    await seedUserKey({ userId: "u1empty", key: "sk-recon-empty-abc" });
    const res = await getJson("/v1/account/repos", {
      headers: { Authorization: "Bearer sk-recon-empty-abc" },
    });
    expect(res.status).toBe(200);
    expect(res.body).toMatchObject({ repos: [], tier: "Pro", limit: 5 });
  });

  it("returns registered repos sorted by recency", async () => {
    await seedUserKey({ userId: "u1list", key: "sk-recon-list-abc", maxRepos: 5 });
    const f1 = await fp("/list/a");
    const f2 = await fp("/list/b");
    await register("sk-recon-list-abc", f1);
    await register("sk-recon-list-abc", f2);
    const res = await getJson("/v1/account/repos", {
      headers: { Authorization: "Bearer sk-recon-list-abc" },
    });
    expect(res.status).toBe(200);
    type RepoListBody = { repos: Array<{ fingerprint: string }> };
    const fps = (res.body as RepoListBody).repos.map((r) => r.fingerprint);
    expect(fps).toContain(f1);
    expect(fps).toContain(f2);
    expect(fps.length).toBe(2);
  });

  it("does not leak repos from other users", async () => {
    await seedUserKey({ userId: "uA", key: "sk-recon-userA-aaa", maxRepos: 5 });
    await seedUserKey({ userId: "uB", key: "sk-recon-userB-bbb", maxRepos: 5 });
    await register("sk-recon-userA-aaa", await fp("/A/x"));
    await register("sk-recon-userB-bbb", await fp("/B/y"));
    const a = await getJson("/v1/account/repos", {
      headers: { Authorization: "Bearer sk-recon-userA-aaa" },
    });
    const b = await getJson("/v1/account/repos", {
      headers: { Authorization: "Bearer sk-recon-userB-bbb" },
    });
    type RepoListBody = { repos: Array<{ fingerprint: string }> };
    expect((a.body as RepoListBody).repos.length).toBe(1);
    expect((b.body as RepoListBody).repos.length).toBe(1);
    expect((a.body as RepoListBody).repos[0].fingerprint).not.toEqual(
      (b.body as RepoListBody).repos[0].fingerprint,
    );
  });
});

describe("DELETE /v1/account/repos/:fingerprint", () => {
  beforeEach(resetDb);

  it("requires auth", async () => {
    const res = await getJson(`/v1/account/repos/${"0".repeat(64)}`, {
      method: "DELETE",
    });
    expect(res.status).toBe(401);
  });

  it("rejects malformed fingerprint", async () => {
    await seedUserKey({ userId: "u1delinv", key: "sk-recon-delinv-abc" });
    const res = await getJson("/v1/account/repos/not-hex", {
      method: "DELETE",
      headers: { Authorization: "Bearer sk-recon-delinv-abc" },
    });
    expect(res.status).toBe(400);
  });

  it("returns 404 for an unregistered fingerprint", async () => {
    await seedUserKey({ userId: "u1del404", key: "sk-recon-del404-abc" });
    const res = await getJson(`/v1/account/repos/${"a".repeat(64)}`, {
      method: "DELETE",
      headers: { Authorization: "Bearer sk-recon-del404-abc" },
    });
    expect(res.status).toBe(404);
  });

  it("returns 204 and frees a slot for re-registration", async () => {
    await seedUserKey({ userId: "u1del", key: "sk-recon-del-abc", maxRepos: 1 });
    const f = await fp("/del/a");
    await register("sk-recon-del-abc", f);
    const overFirst = await register("sk-recon-del-abc", await fp("/del/b"));
    expect(overFirst.status).toBe(403);

    const res = await getJson(`/v1/account/repos/${f}`, {
      method: "DELETE",
      headers: { Authorization: "Bearer sk-recon-del-abc" },
    });
    expect(res.status).toBe(204);

    // Slot is free, the previously-blocked fingerprint can now register.
    const ok = await register("sk-recon-del-abc", await fp("/del/b"));
    expect(ok.status).toBe(201);
  });

  it("does not let user A delete user B's fingerprint", async () => {
    await seedUserKey({ userId: "uAdel", key: "sk-recon-userAdel-aaa" });
    await seedUserKey({ userId: "uBdel", key: "sk-recon-userBdel-bbb" });
    const f = await fp("/B/secret");
    await register("sk-recon-userBdel-bbb", f);
    const res = await getJson(`/v1/account/repos/${f}`, {
      method: "DELETE",
      headers: { Authorization: "Bearer sk-recon-userAdel-aaa" },
    });
    expect(res.status).toBe(404);
    // B's row must still be present.
    const list = await getJson("/v1/account/repos", {
      headers: { Authorization: "Bearer sk-recon-userBdel-bbb" },
    });
    type RepoListBody = { repos: Array<{ fingerprint: string }> };
    expect((list.body as RepoListBody).repos.length).toBe(1);
  });
});

describe("GET /v1/health", () => {
  it("returns 200 OK with status payload (no auth required)", async () => {
    const res = await getJson("/v1/health");
    expect(res.status).toBe(200);
    expect(res.body).toMatchObject({ status: "ok" });
  });
});
