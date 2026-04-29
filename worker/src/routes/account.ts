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

// ── POST /v1/account/savings ──────────────────────────────────────────────────
//
// Accept a daily token-savings rollup from the CLI.
//
// Tier gating: Pro/Team only. Free returns 402. The dashboard panel and the
// CLI both surface the upgrade path when this fires.
//
// Idempotency / monotonicity: the upsert is MAX-merged. A repeat push with
// the same `day` cannot regress an already-stored counter — even if the
// client's local DB rolled back or a stale daemon retried after a fresh
// push. The trade is "may slightly under-credit on machines that re-create
// the local DB," which is the right side of the trade for the dashboard
// (we'd rather understate than overstate savings).

/** Validate YYYY-MM-DD UTC date string. Strict: catches typos before
 *  they wedge a bad row into the table. */
function validDay(d: unknown): d is string {
  if (typeof d !== "string" || d.length !== 10) return false;
  if (!/^\d{4}-\d{2}-\d{2}$/.test(d)) return false;
  const t = Date.parse(d + "T00:00:00Z");
  return Number.isFinite(t);
}

/** Validate that a counter looks sane: non-negative, finite, integer-shaped.
 *  Caps each at 2^53 to stay inside Number safe-int range; D1 stores i64
 *  but JSON.parse hands us f64 and rounding above 2^53 silently corrupts. */
function validCount(n: unknown): n is number {
  return (
    typeof n === "number" &&
    Number.isFinite(n) &&
    Number.isInteger(n) &&
    n >= 0 &&
    n <= Number.MAX_SAFE_INTEGER
  );
}

/** Tier-gate: Pro and Team can push; Free and unknowns cannot. Mirrors
 *  the worker's tier strings in `lib/tiers.ts` and the api_keys.tier
 *  column. Treats any non-recognised tier as Free for safety. */
function canPushSavings(tier: string): boolean {
  return tier === "Pro" || tier === "Team" || tier === "Enterprise";
}

/**
 * Optional `repo_fingerprint` validator for the savings push.
 *
 * New CLIs (v0.3.3+) send the same SHA-256 path fingerprint that
 * `recon init` registers via /v1/account/repos. Older CLIs omit it; we
 * default to `''` (empty string) so their pushes land in the legacy
 * bucket alongside any pre-v0.3.3 rows.
 *
 * Differs from `validFingerprint` (the strict variant used by repo
 * register/remove): here `undefined`, `null`, and `''` are all valid
 * and route to the legacy bucket. Anything else must be a 64-character
 * lowercase hex string — same shape as a real fingerprint, since we
 * store it directly in a primary-key column.
 */
function validOptionalFingerprint(v: unknown): boolean {
  if (v === undefined || v === null || v === "") return true;
  return typeof v === "string" && /^[0-9a-f]{64}$/.test(v);
}

accountRoutes.post("/savings", async (c) => {
  let body: {
    day?: unknown;
    repo_fingerprint?: unknown;
    calls?: unknown;
    response_tokens?: unknown;
    baseline_tokens?: unknown;
    tokens_saved?: unknown;
    latency_micros?: unknown;
  };
  try {
    body = await c.req.json();
  } catch {
    return c.json({ error: "Invalid JSON body" }, 400);
  }

  if (!validDay(body.day)) {
    return c.json(
      { error: "day must be a YYYY-MM-DD UTC date string" },
      400,
    );
  }
  if (!validOptionalFingerprint(body.repo_fingerprint)) {
    return c.json(
      {
        error:
          "repo_fingerprint, when supplied, must be a 64-character lowercase hex string",
      },
      400,
    );
  }
  if (
    !validCount(body.calls) ||
    !validCount(body.response_tokens) ||
    !validCount(body.baseline_tokens) ||
    !validCount(body.tokens_saved) ||
    !validCount(body.latency_micros)
  ) {
    return c.json(
      {
        error:
          "calls, response_tokens, baseline_tokens, tokens_saved, latency_micros must be non-negative integers",
      },
      400,
    );
  }
  // Default missing/null to '' (legacy bucket). The route already widened
  // the type to accept undefined; the column has the same default at the
  // SQL level, but binding explicit values keeps the prepared-statement
  // shape stable across old and new clients.
  const repoFingerprint =
    typeof body.repo_fingerprint === "string" ? body.repo_fingerprint : "";

  const user = c.get("user");
  const apiKey = c.get("apiKey");

  if (!canPushSavings(apiKey.tier)) {
    return c.json(
      {
        error: "savings_push_requires_pro",
        tier: apiKey.tier,
        message:
          "The savings dashboard is a Pro/Team feature. Upgrade your plan at https://mcprecon.pages.dev/pricing to enable usage rollups.",
      },
      402,
    );
  }

  const db = c.env.RECON_DB;

  // Single statement: insert the row, or MAX-merge each counter on
  // conflict. SQLite's `excluded.col` references the proposed-but-conflicting
  // values; `MAX(existing, proposed)` makes pushes monotone. updated_at is
  // refreshed unconditionally so we have a "last seen" timestamp even when
  // the counters didn't move.
  await db
    .prepare(
      `INSERT INTO usage_rollups
         (user_id, repo_fingerprint, day, calls, response_tokens, baseline_tokens, tokens_saved, latency_micros)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(user_id, repo_fingerprint, day) DO UPDATE SET
         calls           = MAX(usage_rollups.calls,           excluded.calls),
         response_tokens = MAX(usage_rollups.response_tokens, excluded.response_tokens),
         baseline_tokens = MAX(usage_rollups.baseline_tokens, excluded.baseline_tokens),
         tokens_saved    = MAX(usage_rollups.tokens_saved,    excluded.tokens_saved),
         latency_micros  = MAX(usage_rollups.latency_micros,  excluded.latency_micros),
         updated_at      = datetime('now')`,
    )
    .bind(
      user.id,
      repoFingerprint,
      body.day,
      body.calls,
      body.response_tokens,
      body.baseline_tokens,
      body.tokens_saved,
      body.latency_micros,
    )
    .run();

  return c.json({
    status: "recorded",
    day: body.day,
    tier: apiKey.tier,
  });
});
