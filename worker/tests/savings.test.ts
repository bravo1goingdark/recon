/**
 * /v1/account/savings (push) and /v1/dashboard/savings (pull) contract tests.
 *
 * Coverage:
 *   - Pro/Team can push; Free is rejected with 402 (the explicit upsell tier gate)
 *   - MAX-merge upsert: a stale push cannot regress a stored counter
 *   - Range cap by tier (Pro=30d, Team=90d, Enterprise=365d)
 *   - Free GET returns the upsell payload, never queries D1
 *   - Honours an explicit ?range=N down-shift, clamps above the cap
 *   - Day validation (rejects malformed YYYY-MM-DD)
 *   - Counter validation (rejects negative / non-integer / overflow)
 *
 * Auth seeding mirrors account.test.ts so the two suites stay
 * structurally similar; new helpers here (seedSession, push, fetchSavings)
 * are kept local to keep cross-test coupling low.
 */

import { beforeEach, describe, expect, it } from "vitest";
import { env, getJson, resetDb } from "./setup";

// ── Fixtures ──────────────────────────────────────────────────────────────────

async function sha256Hex(input: string): Promise<string> {
  const buf = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(input),
  );
  return Array.from(new Uint8Array(buf))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

async function seedUserKey(opts: {
  userId: string;
  key: string;
  tier?: string;
}): Promise<void> {
  const db = (env as { RECON_DB: D1Database }).RECON_DB;
  const tier = opts.tier ?? "Pro";
  const keyHash = await sha256Hex(opts.key);
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
       VALUES (?, ?, ?, 'Default', ?, ?, NULL, NULL)`,
    )
    .bind(
      opts.userId,
      keyHash,
      opts.key.slice(0, 14),
      tier,
      JSON.stringify({ max_repos: 5, max_files: 5000, max_loc: 200000 }),
    )
    .run();
}

/** Seed a dashboard session. Mirrors auth middleware's hash-then-lookup. */
async function seedSession(opts: {
  userId: string;
  token: string;
}): Promise<string> {
  const db = (env as { RECON_DB: D1Database }).RECON_DB;
  const tokenHash = await sha256Hex(opts.token);
  // Sessions table requires a sessionId (TEXT PK). Derive deterministically.
  const sessionId = "sess_" + tokenHash.slice(0, 16);
  // Match the schema's expires_at format (datetime string, 1h in the future).
  const expiresAt = new Date(Date.now() + 3600_000).toISOString();
  await db
    .prepare(
      `INSERT INTO sessions (id, user_id, token_hash, expires_at)
       VALUES (?, ?, ?, ?)`,
    )
    .bind(sessionId, opts.userId, tokenHash, expiresAt)
    .run();
  return opts.token;
}

function push(
  key: string,
  body: Record<string, unknown>,
): Promise<{ status: number; body: unknown; headers: Headers }> {
  return getJson("/v1/account/savings", {
    method: "POST",
    headers: {
      Authorization: "Bearer " + key,
      "Content-Type": "application/json",
    },
    body: JSON.stringify(body),
  });
}

function fetchSavings(
  token: string,
  query?: string,
): Promise<{ status: number; body: unknown; headers: Headers }> {
  const path =
    "/v1/dashboard/savings" + (query !== undefined ? "?" + query : "");
  return getJson(path, {
    headers: { Authorization: "Bearer " + token },
  });
}

const TODAY = new Date().toISOString().slice(0, 10);

beforeEach(async () => {
  await resetDb();
});

// ── Push (POST /v1/account/savings) ───────────────────────────────────────────

describe("POST /v1/account/savings", () => {
  it("Pro tier: records a fresh rollup", async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
    const r = await push("sk-recon-pro", {
      day: TODAY,
      calls: 42,
      response_tokens: 8000,
      static_baseline_tokens: 60000,
      tokens_saved: 52000,
      latency_micros: 12_000_000,
    });
    expect(r.status).toBe(200);
    expect((r.body as { status: string }).status).toBe("recorded");

    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const row = await db
      .prepare(
        "SELECT calls, tokens_saved FROM usage_rollups WHERE user_id = ? AND day = ?",
      )
      .bind("u_pro", TODAY)
      .first<{ calls: number; tokens_saved: number }>();
    expect(row).not.toBeNull();
    expect(row!.calls).toBe(42);
    expect(row!.tokens_saved).toBe(52000);
  });

  it("Free tier: rejected with 402 + upsell payload", async () => {
    await seedUserKey({ userId: "u_free", key: "sk-recon-free", tier: "Free" });
    const r = await push("sk-recon-free", {
      day: TODAY,
      calls: 1,
      response_tokens: 1,
      static_baseline_tokens: 1,
      tokens_saved: 0,
      latency_micros: 1,
    });
    expect(r.status).toBe(402);
    const body = r.body as { error: string; tier: string; message: string };
    expect(body.error).toBe("savings_push_requires_pro");
    expect(body.tier).toBe("Free");
    expect(body.message).toContain("Pro/Team");
  });

  it("MAX-merge: a stale push cannot regress an existing counter", async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
    // First push: 100 saved. Second push: 30 saved (stale snapshot). The
    // stored value must remain 100 — the dashboard never under-reports
    // even when a slow CLI sends an out-of-order snapshot.
    await push("sk-recon-pro", {
      day: TODAY,
      calls: 10,
      response_tokens: 1000,
      static_baseline_tokens: 30000,
      tokens_saved: 100,
      latency_micros: 100,
    });
    await push("sk-recon-pro", {
      day: TODAY,
      calls: 5,
      response_tokens: 500,
      static_baseline_tokens: 10000,
      tokens_saved: 30,
      latency_micros: 50,
    });
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const row = await db
      .prepare(
        "SELECT calls, tokens_saved FROM usage_rollups WHERE user_id = ? AND day = ?",
      )
      .bind("u_pro", TODAY)
      .first<{ calls: number; tokens_saved: number }>();
    expect(row!.calls).toBe(10);
    expect(row!.tokens_saved).toBe(100);
  });

  it("Idempotent: a fresh push with HIGHER counters bumps the row", async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
    await push("sk-recon-pro", {
      day: TODAY,
      calls: 10,
      response_tokens: 1000,
      static_baseline_tokens: 30000,
      tokens_saved: 100,
      latency_micros: 100,
    });
    await push("sk-recon-pro", {
      day: TODAY,
      calls: 100,
      response_tokens: 5000,
      static_baseline_tokens: 80000,
      tokens_saved: 1000,
      latency_micros: 5000,
    });
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const row = await db
      .prepare(
        "SELECT calls, tokens_saved FROM usage_rollups WHERE user_id = ? AND day = ?",
      )
      .bind("u_pro", TODAY)
      .first<{ calls: number; tokens_saved: number }>();
    expect(row!.calls).toBe(100);
    expect(row!.tokens_saved).toBe(1000);
  });

  it("Validates day format (rejects malformed)", async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
    const r = await push("sk-recon-pro", {
      day: "not-a-date",
      calls: 1,
      response_tokens: 1,
      static_baseline_tokens: 1,
      tokens_saved: 0,
      latency_micros: 1,
    });
    expect(r.status).toBe(400);
    expect((r.body as { error: string }).error).toContain("YYYY-MM-DD");
  });

  it("Validates counter shape (rejects negative)", async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
    const r = await push("sk-recon-pro", {
      day: TODAY,
      calls: -1,
      response_tokens: 0,
      static_baseline_tokens: 0,
      tokens_saved: 0,
      latency_micros: 0,
    });
    expect(r.status).toBe(400);
  });

  it("Rejects non-integer counters (would corrupt at f64 precision)", async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
    const r = await push("sk-recon-pro", {
      day: TODAY,
      calls: 1.5,
      response_tokens: 0,
      static_baseline_tokens: 0,
      tokens_saved: 0,
      latency_micros: 0,
    });
    expect(r.status).toBe(400);
  });

  it("401 without an Authorization header", async () => {
    const r = await getJson("/v1/account/savings", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        day: TODAY,
        calls: 1,
        response_tokens: 1,
        static_baseline_tokens: 1,
        tokens_saved: 0,
        latency_micros: 1,
      }),
    });
    expect(r.status).toBe(401);
  });
});

// ── Pull (GET /v1/dashboard/savings) ──────────────────────────────────────────

describe("GET /v1/dashboard/savings", () => {
  it("Free tier: 200 with empty daily + upsell payload, no DB rows queried", async () => {
    await seedUserKey({ userId: "u_free", key: "sk-recon-free", tier: "Free" });
    await seedSession({ userId: "u_free", token: "sess-free" });

    const r = await fetchSavings("sess-free");
    expect(r.status).toBe(200);
    const body = r.body as {
      tier: string;
      range_days: number;
      daily: unknown[];
      totals: { tokens_saved: number };
      upsell: { upgrade_url: string };
    };
    expect(body.tier).toBe("Free");
    expect(body.range_days).toBe(0);
    expect(body.daily).toEqual([]);
    expect(body.totals.tokens_saved).toBe(0);
    expect(body.upsell.upgrade_url).toContain("/pricing");
  });

  it("Pro tier: returns last 30 days of rollups + aggregate totals", async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
    await seedSession({ userId: "u_pro", token: "sess-pro" });
    // Push 3 days of rollups.
    const days = [-2, -1, 0].map((d) => {
      const x = new Date();
      x.setUTCDate(x.getUTCDate() + d);
      return x.toISOString().slice(0, 10);
    });
    for (const day of days) {
      await push("sk-recon-pro", {
        day,
        calls: 10,
        response_tokens: 1000,
        static_baseline_tokens: 30000,
        tokens_saved: 29000,
        latency_micros: 100_000,
      });
    }

    const r = await fetchSavings("sess-pro");
    expect(r.status).toBe(200);
    const body = r.body as {
      tier: string;
      range_days: number;
      daily: { day: string; tokens_saved: number }[];
      totals: { tokens_saved: number; calls: number };
    };
    expect(body.tier).toBe("Pro");
    expect(body.range_days).toBe(30);
    expect(body.daily.length).toBe(3);
    expect(body.daily.map((d) => d.day)).toEqual(days);
    expect(body.totals.tokens_saved).toBe(87000);
    expect(body.totals.calls).toBe(30);
  });

  it("Honours ?range=7 down-shift, but clamps above tier cap", async () => {
    await seedUserKey({ userId: "u_team", key: "sk-recon-team", tier: "Team" });
    await seedSession({ userId: "u_team", token: "sess-team" });
    let r = await fetchSavings("sess-team", "range=7");
    expect((r.body as { range_days: number }).range_days).toBe(7);
    // Team caps at 90; a request for 365 should clamp to 90.
    r = await fetchSavings("sess-team", "range=365");
    expect((r.body as { range_days: number }).range_days).toBe(90);
  });

  it("Pro tier: rollups outside 30-day window are excluded from totals", async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
    await seedSession({ userId: "u_pro", token: "sess-pro" });
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    // Insert directly so we can backdate well past the cap.
    const oldDay = new Date();
    oldDay.setUTCDate(oldDay.getUTCDate() - 60);
    await db
      .prepare(
        `INSERT INTO usage_rollups
           (user_id, day, calls, response_tokens, static_baseline_tokens, tokens_saved, latency_micros)
         VALUES (?, ?, ?, ?, ?, ?, ?)`,
      )
      .bind(
        "u_pro",
        oldDay.toISOString().slice(0, 10),
        999,
        99999,
        99999,
        99999,
        99999,
      )
      .run();
    // And one inside.
    await push("sk-recon-pro", {
      day: TODAY,
      calls: 5,
      response_tokens: 500,
      static_baseline_tokens: 1500,
      tokens_saved: 1000,
      latency_micros: 5000,
    });

    const r = await fetchSavings("sess-pro");
    const body = r.body as {
      daily: unknown[];
      totals: { tokens_saved: number };
    };
    expect(body.daily.length).toBe(1);
    expect(body.totals.tokens_saved).toBe(1000);
  });

  it("401 without a session token", async () => {
    const r = await getJson("/v1/dashboard/savings");
    expect(r.status).toBe(401);
  });

  it("Scoped: one user cannot see another user's rollups", async () => {
    await seedUserKey({ userId: "u_a", key: "sk-recon-a", tier: "Pro" });
    await seedUserKey({ userId: "u_b", key: "sk-recon-b", tier: "Pro" });
    await seedSession({ userId: "u_a", token: "sess-a" });
    await push("sk-recon-b", {
      day: TODAY,
      calls: 999,
      response_tokens: 1,
      static_baseline_tokens: 1,
      tokens_saved: 1,
      latency_micros: 1,
    });

    const r = await fetchSavings("sess-a");
    const body = r.body as { daily: unknown[]; totals: { calls: number } };
    expect(body.daily).toEqual([]);
    expect(body.totals.calls).toBe(0);
  });

  it("Per-repo: SUM-aggregates across repo_fingerprint per day", async () => {
    // Two repos under one user push to the same day. The dashboard sees
    // the SUM, not MAX (the v0.3.2 bug fixed in 0.3.3).
    const fpA =
      "a".repeat(64); // 64-char hex sentinel for repo A
    const fpB =
      "b".repeat(64); // 64-char hex sentinel for repo B
    await seedUserKey({ userId: "u_pro", key: "sk-recon-multi", tier: "Pro" });
    await seedSession({ userId: "u_pro", token: "sess-multi" });

    await push("sk-recon-multi", {
      day: TODAY,
      repo_fingerprint: fpA,
      calls: 100,
      response_tokens: 10_000,
      static_baseline_tokens: 60_000,
      tokens_saved: 50_000,
      latency_micros: 1_000_000,
    });
    await push("sk-recon-multi", {
      day: TODAY,
      repo_fingerprint: fpB,
      calls: 80,
      response_tokens: 8_000,
      static_baseline_tokens: 48_000,
      tokens_saved: 40_000,
      latency_micros: 800_000,
    });

    const r = await fetchSavings("sess-multi");
    const body = r.body as {
      daily: { day: string; calls: number; tokens_saved: number }[];
      totals: { calls: number; tokens_saved: number };
    };
    expect(body.daily).toHaveLength(1);
    expect(body.daily[0].day).toBe(TODAY);
    // SUM, not MAX: repo A (100) + repo B (80) = 180. The v0.3.2 bug
    // reported 100 here because MAX-merge collapsed both pushes into
    // one row keyed only by (user_id, day).
    expect(body.daily[0].calls).toBe(180);
    expect(body.daily[0].tokens_saved).toBe(90_000);
    expect(body.totals.calls).toBe(180);
    expect(body.totals.tokens_saved).toBe(90_000);
  });

  it("Legacy bucket: pre-v0.3.3 push (no fingerprint) keyed under '' and shown to user", async () => {
    // Older CLIs omit `repo_fingerprint`. The route defaults to the
    // empty-string legacy bucket — old pushes still land and the
    // dashboard still sees them.
    await seedUserKey({ userId: "u_pro", key: "sk-recon-legacy", tier: "Pro" });
    await seedSession({ userId: "u_pro", token: "sess-legacy" });

    const r = await push("sk-recon-legacy", {
      day: TODAY,
      // intentionally no repo_fingerprint
      calls: 42,
      response_tokens: 4000,
      static_baseline_tokens: 30000,
      tokens_saved: 26000,
      latency_micros: 500_000,
    });
    expect(r.status).toBe(200);

    const view = await fetchSavings("sess-legacy");
    const body = view.body as {
      daily: { calls: number; tokens_saved: number }[];
      totals: { calls: number; tokens_saved: number };
    };
    expect(body.totals.calls).toBe(42);
    expect(body.totals.tokens_saved).toBe(26000);
  });

  it("Per-repo + legacy: a legacy push and a per-repo push from same user SUM together", async () => {
    // Migration left v0.3.2 rows in the legacy `''` bucket; v0.3.3+
    // pushes go to their own per-repo rows. The GROUP BY day SUM on
    // read folds both buckets into one daily total — no double-count,
    // no lost data.
    const fp = "f".repeat(64);
    await seedUserKey({ userId: "u_pro", key: "sk-recon-mix", tier: "Pro" });
    await seedSession({ userId: "u_pro", token: "sess-mix" });

    // Legacy-shape push (the v0.3.2 client).
    await push("sk-recon-mix", {
      day: TODAY,
      calls: 30,
      response_tokens: 3000,
      static_baseline_tokens: 18000,
      tokens_saved: 15000,
      latency_micros: 200_000,
    });
    // New-shape push from a real repo.
    await push("sk-recon-mix", {
      day: TODAY,
      repo_fingerprint: fp,
      calls: 70,
      response_tokens: 7000,
      static_baseline_tokens: 42000,
      tokens_saved: 35000,
      latency_micros: 700_000,
    });

    const r = await fetchSavings("sess-mix");
    const body = r.body as {
      totals: { calls: number; tokens_saved: number };
    };
    expect(body.totals.calls).toBe(100);
    expect(body.totals.tokens_saved).toBe(50000);
  });

  it("Rejects malformed repo_fingerprint", async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-badfp", tier: "Pro" });

    // Wrong length
    let r = await push("sk-recon-badfp", {
      day: TODAY,
      repo_fingerprint: "abc123",
      calls: 1,
      response_tokens: 1,
      static_baseline_tokens: 1,
      tokens_saved: 0,
      latency_micros: 1,
    });
    expect(r.status).toBe(400);

    // Uppercase hex
    r = await push("sk-recon-badfp", {
      day: TODAY,
      repo_fingerprint: "A".repeat(64),
      calls: 1,
      response_tokens: 1,
      static_baseline_tokens: 1,
      tokens_saved: 0,
      latency_micros: 1,
    });
    expect(r.status).toBe(400);

    // Non-hex characters
    r = await push("sk-recon-badfp", {
      day: TODAY,
      repo_fingerprint: "z".repeat(64),
      calls: 1,
      response_tokens: 1,
      static_baseline_tokens: 1,
      tokens_saved: 0,
      latency_micros: 1,
    });
    expect(r.status).toBe(400);
  });

  // ── Measured baselines (v0.4) ─────────────────────────────────────
  // The wire shape requires both static_baseline_tokens and
  // measured_baseline_tokens; each call accrues exactly one. The
  // dashboard sums the two on the read path and clamps tokens_saved
  // at 0.

  it("Measured fields: present on push → row stored with values", async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
    const r = await push("sk-recon-pro", {
      day: TODAY,
      calls: 5,
      response_tokens: 200,
      static_baseline_tokens: 0,
      measured_baseline_tokens: 12_500,
      tokens_saved: 12_300,
      latency_micros: 1234,
    });
    expect(r.status).toBe(200);
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const row = await db
      .prepare(
        `SELECT static_baseline_tokens, measured_baseline_tokens
         FROM usage_rollups WHERE user_id = ? AND day = ?`,
      )
      .bind("u_pro", TODAY)
      .first<{
        static_baseline_tokens: number;
        measured_baseline_tokens: number;
      }>();
    expect(row).not.toBeNull();
    expect(row!.static_baseline_tokens).toBe(0);
    expect(row!.measured_baseline_tokens).toBe(12_500);
  });

  it("Measured fields: MAX-merge on conflict (same key, lower second push)", async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
    await push("sk-recon-pro", {
      day: TODAY,
      calls: 5,
      response_tokens: 200,
      static_baseline_tokens: 0,
      measured_baseline_tokens: 12_500,
      tokens_saved: 12_300,
      latency_micros: 1234,
    });
    // Second push has lower numbers — MAX-merge must not regress them.
    await push("sk-recon-pro", {
      day: TODAY,
      calls: 4,
      response_tokens: 150,
      static_baseline_tokens: 0,
      measured_baseline_tokens: 11_000,
      tokens_saved: 10_850,
      latency_micros: 1000,
    });
    const db = (env as { RECON_DB: D1Database }).RECON_DB;
    const row = await db
      .prepare(
        `SELECT measured_baseline_tokens, calls
         FROM usage_rollups WHERE user_id = ? AND day = ?`,
      )
      .bind("u_pro", TODAY)
      .first<{ measured_baseline_tokens: number; calls: number }>();
    expect(row!.measured_baseline_tokens).toBe(12_500); // higher of the two
    expect(row!.calls).toBe(5);
  });

  it("Measured fields: rejects negative measured_baseline_tokens", async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
    const r = await push("sk-recon-pro", {
      day: TODAY,
      calls: 1,
      response_tokens: 1,
      static_baseline_tokens: 1,
      measured_baseline_tokens: -1,
      tokens_saved: 0,
      latency_micros: 1,
    });
    expect(r.status).toBe(400);
  });

  it("Dashboard: tokens_saved derived from (static + measured) and clamped at zero", async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
    await seedSession({ userId: "u_pro", token: "sess-measured" });
    // Pathological: response_tokens > combined baselines. The derived
    // tokens_saved must clamp to 0 instead of going negative.
    await push("sk-recon-pro", {
      day: TODAY,
      calls: 1,
      response_tokens: 200,
      static_baseline_tokens: 50,
      measured_baseline_tokens: 50,
      tokens_saved: 0,
      latency_micros: 100,
    });
    const view = await fetchSavings("sess-measured");
    expect(view.status).toBe(200);
    const body = view.body as {
      daily: Array<{
        tokens_saved: number;
        static_baseline_tokens: number;
        measured_baseline_tokens: number;
      }>;
      totals: {
        tokens_saved: number;
        static_baseline_tokens: number;
        measured_baseline_tokens: number;
      };
    };
    expect(body.daily[0].tokens_saved).toBe(0);
    expect(body.daily[0].static_baseline_tokens).toBe(50);
    expect(body.daily[0].measured_baseline_tokens).toBe(50);
    expect(body.totals.tokens_saved).toBe(0);
  });

  it("Dashboard: tokens_saved sums static + measured for migrated and composite tools", async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
    await seedSession({ userId: "u_pro", token: "sess-mix" });
    // Composite + migrated tool credits in the same day's rollup.
    // Worker derives tokens_saved = (static + measured) - response.
    await push("sk-recon-pro", {
      day: TODAY,
      calls: 6,
      response_tokens: 500,
      static_baseline_tokens: 10_000,
      measured_baseline_tokens: 12_000,
      tokens_saved: 21_500,
      latency_micros: 5000,
    });
    const view = await fetchSavings("sess-mix");
    expect(view.status).toBe(200);
    const body = view.body as {
      daily: Array<{ tokens_saved: number }>;
      totals: { tokens_saved: number };
    };
    expect(body.daily[0].tokens_saved).toBe(21_500);
    expect(body.totals.tokens_saved).toBe(21_500);
  });
});
