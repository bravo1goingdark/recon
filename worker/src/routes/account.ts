/**
 * /v1/account/* — server-side repo registry.
 *
 * v0.2.0 moves `max_repos` enforcement from the client (where a patched
 * binary trivially bypasses it) to the worker. The CLI registers each
 * `recon init`'d repo by its canonical path's SHA-256 fingerprint; the
 * worker rejects registrations that would push the user past their
 * tier-defined `max_repos`.
 *
 * Endpoints:
 *   POST   /v1/account/repos             { fingerprint } → 201 / 200 / 403
 *   GET    /v1/account/repos             → { repos: [{ fingerprint, first_seen_at, last_seen_at }] }
 *   DELETE /v1/account/repos/:fingerprint → 204 / 404
 *
 * Authentication: API-key Bearer (apikey middleware), mirroring
 * /v1/license/validate.  Rate-limited per key prefix via RL_ACCOUNT —
 * normal traffic is one POST per `recon init`, so 60/min has six
 * orders of magnitude of headroom.
 *
 * Atomicity: the POST uses a single `INSERT … SELECT … WHERE` with the
 * count check inside the WHERE clause, so two concurrent POSTs from the
 * same user with different fingerprints can't both pass when the user
 * is at `max_repos - 1`. SQLite (and therefore D1) evaluates the WHERE
 * predicate inside the same statement-level transaction as the INSERT.
 */

import { Hono } from "hono";
import { rateLimit } from "../middleware/ratelimit";
import { apiKeyRateKey, requireApiKey } from "../middleware/apikey";
import type { ApiKeyVars } from "../middleware/apikey";
import type { Env } from "../types";

type AccountEnv = { Bindings: Env; Variables: ApiKeyVars };

export const accountRoutes = new Hono<AccountEnv>();

accountRoutes.use("*", rateLimit("RL_ACCOUNT", apiKeyRateKey, 60));
accountRoutes.use("*", requireApiKey);

// ── Helpers ───────────────────────────────────────────────────────────────────

/** Validate that a fingerprint is a 64-char lowercase hex string (SHA-256). */
function validFingerprint(fp: unknown): fp is string {
  return typeof fp === "string" && /^[0-9a-f]{64}$/.test(fp);
}

/** Read tier-defined max_repos from the api_keys row attached by the middleware. */
function maxReposFor(apiKey: ApiKeyVars["apiKey"]): number {
  try {
    const limits = JSON.parse(apiKey.limits_json) as { max_repos?: number };
    if (typeof limits.max_repos === "number" && limits.max_repos > 0) {
      return limits.max_repos;
    }
  } catch {
    /* fall through */
  }
  // Conservative default if limits_json is malformed: 1 repo. Better to
  // surface the issue (user will see 403 on a 2nd repo) than to silently
  // grant unlimited.
  return 1;
}

// ── POST /v1/account/repos ────────────────────────────────────────────────────

accountRoutes.post("/repos", async (c) => {
  let body: { fingerprint?: unknown };
  try {
    body = await c.req.json();
  } catch {
    return c.json({ error: "Invalid JSON body" }, 400);
  }

  const fp = body.fingerprint;
  if (!validFingerprint(fp)) {
    return c.json(
      { error: "fingerprint must be 64-char lowercase hex (SHA-256)" },
      400,
    );
  }

  const user = c.get("user");
  const apiKey = c.get("apiKey");
  const max = maxReposFor(apiKey);
  const db = c.env.RECON_DB;

  // Pre-check existence — needed to distinguish 201 (new) vs 200 (refresh).
  // The conditional INSERT below is the actual atomic gate; this read is
  // for response-shape only, not for limit enforcement, so a stale read
  // racing with a parallel POST cannot cause an over-quota write.
  const existed = await db
    .prepare(
      "SELECT 1 AS one FROM user_repos WHERE user_id = ? AND fingerprint = ?",
    )
    .bind(user.id, fp)
    .first<{ one: number }>();

  // Atomic conditional upsert.
  //   - WHERE fires inside the INSERT-SELECT, evaluated against the same
  //     snapshot the INSERT writes against → no read/write race.
  //   - ON CONFLICT bumps last_seen_at when the row already exists, so a
  //     repeat POST is a free idempotency token.
  //   - meta.changes:
  //       1 — inserted OR ON CONFLICT-updated (request honoured)
  //       0 — not allowed (over limit AND not already present)
  const result = await db
    .prepare(
      `INSERT INTO user_repos (user_id, fingerprint)
         SELECT ?, ?
         WHERE EXISTS (SELECT 1 FROM user_repos WHERE user_id = ? AND fingerprint = ?)
            OR (SELECT COUNT(*) FROM user_repos WHERE user_id = ?) < ?
       ON CONFLICT(user_id, fingerprint) DO UPDATE SET last_seen_at = datetime('now')`,
    )
    .bind(user.id, fp, user.id, fp, user.id, max)
    .run();

  if (result.meta.changes === 0) {
    return c.json(
      {
        error: "max_repos exceeded",
        limit: max,
        tier: apiKey.tier,
        message: `Your ${apiKey.tier} plan allows ${max} repo${
          max === 1 ? "" : "s"
        }. Free a slot with \`recon repos remove <path>\` or upgrade your plan.`,
      },
      403,
    );
  }

  return c.json(
    {
      fingerprint: fp,
      status: existed ? "refreshed" : "registered",
      limit: max,
      tier: apiKey.tier,
    },
    existed ? 200 : 201,
  );
});

// ── GET /v1/account/repos ─────────────────────────────────────────────────────

accountRoutes.get("/repos", async (c) => {
  const user = c.get("user");
  const apiKey = c.get("apiKey");
  const max = maxReposFor(apiKey);
  const db = c.env.RECON_DB;

  const rows = await db
    .prepare(
      `SELECT fingerprint, first_seen_at, last_seen_at
       FROM user_repos
       WHERE user_id = ?
       ORDER BY last_seen_at DESC`,
    )
    .bind(user.id)
    .all<{ fingerprint: string; first_seen_at: string; last_seen_at: string }>();

  return c.json({
    repos: rows.results ?? [],
    limit: max,
    tier: apiKey.tier,
  });
});

// ── DELETE /v1/account/repos/:fingerprint ─────────────────────────────────────

accountRoutes.delete("/repos/:fingerprint", async (c) => {
  const fp = c.req.param("fingerprint");
  if (!validFingerprint(fp)) {
    return c.json(
      { error: "fingerprint must be 64-char lowercase hex (SHA-256)" },
      400,
    );
  }

  const user = c.get("user");
  const db = c.env.RECON_DB;

  const result = await db
    .prepare("DELETE FROM user_repos WHERE user_id = ? AND fingerprint = ?")
    .bind(user.id, fp)
    .run();

  if (result.meta.changes === 0) {
    return c.json({ error: "fingerprint not registered" }, 404);
  }
  return new Response(null, { status: 204 });
});
