import { Hono } from "hono";
import { sha256Hex, generateApiKey } from "../lib/crypto";
import { cancelSubscription } from "../lib/razorpay";
import { getTierConfig } from "../lib/tiers";
import { requireAuth } from "../middleware/auth";
import { clientIp, rateLimit } from "../middleware/ratelimit";
import { KeyCreateBody, parseBody } from "../schemas";
import type { AuthUser, Env } from "../types";

type AuthedEnv = { Bindings: Env; Variables: { user: AuthUser } };

export const dashboardRoutes = new Hono<AuthedEnv>();

// All dashboard routes require auth + are rate-limited per authenticated user.
dashboardRoutes.use("*", requireAuth);
dashboardRoutes.use(
  "*",
  rateLimit<AuthedEnv>(
    "RL_DASHBOARD",
    (c) => c.get("user")?.id ?? clientIp(c),
    60,
  ),
);

/** GET /v1/dashboard/keys — list user's API keys. */
dashboardRoutes.get("/keys", async (c) => {
  const user = c.get("user");
  const db = c.env.RECON_DB;

  const { results } = await db
    .prepare(
      `SELECT id, key_prefix, name, tier, limits_json, expires_at, created_at, revoked_at
       FROM api_keys WHERE user_id = ? ORDER BY created_at DESC`,
    )
    .bind(user.id)
    .all();

  const keys = (results ?? []).map((row) => ({
    id: row.id,
    key_prefix: row.key_prefix,
    name: row.name,
    tier: row.tier,
    limits: JSON.parse(row.limits_json as string),
    expires_at: row.expires_at,
    created_at: row.created_at,
    revoked: !!row.revoked_at,
  }));

  return c.json({ keys });
});

/**
 * POST /v1/dashboard/keys — generate a new API key.
 *
 * One active key per account. If the user already has a non-revoked key,
 * the request 409s with the existing key's prefix so the UI can prompt
 * the user to rotate (revoke + regenerate) instead of silently stacking
 * keys. Prevents a single subscriber from provisioning a key per
 * team-mate and bypassing the per-account tier limits.
 *
 * Rotation path: DELETE /v1/dashboard/keys/:id → POST /v1/dashboard/keys
 * in two calls. The dashboard's "Rotate key" button wraps both in a
 * single modal confirmation.
 */
dashboardRoutes.post("/keys", async (c) => {
  const user = c.get("user");
  const db = c.env.RECON_DB;

  const existing = await db
    .prepare(
      `SELECT id, key_prefix FROM api_keys
       WHERE user_id = ? AND revoked_at IS NULL
       LIMIT 1`,
    )
    .bind(user.id)
    .first<{ id: string; key_prefix: string }>();
  if (existing) {
    return c.json(
      {
        error:
          "One active key per account. Revoke the existing key before generating a new one.",
        existing_key_id: existing.id,
        existing_key_prefix: existing.key_prefix,
      },
      409,
    );
  }

  // Name is optional in the product UX but when provided must match the
  // Zod-constrained charset (letters/digits/space/_./-). Empty body → default.
  let name = "Default";
  try {
    const raw = await c.req.json();
    const parsed = parseBody(KeyCreateBody, raw);
    if (!parsed.ok) return c.json(parsed.error, 400);
    name = parsed.value.name;
  } catch {
    // Body was empty — keep default name.
  }

  const key = generateApiKey();
  const keyHash = await sha256Hex(key);
  const keyPrefix = key.slice(0, 14);
  const tierConfig = getTierConfig(user.tier);

  // If the user has an active paid subscription, pin the new key's
  // expires_at to that subscription's current_period_end. Without this
  // the hourly downgrade cron (src/scheduled.ts) would never flip the
  // key back to Free — its WHERE clause excludes NULL expires_at, so a
  // sub that ends leaves the key permanently at its paid tier.
  //
  // `active` / `halted` / `cancelled` all have a known period_end with
  // paid access through that date. Other statuses (created, authenticated,
  // pending, expired, completed) either have no paid period yet or the
  // period has already lapsed — leave expires_at NULL and let the user
  // stay on whatever tier users.tier currently reflects.
  const paidSub = await db
    .prepare(
      `SELECT current_period_end FROM subscriptions
       WHERE user_id = ?
         AND status IN ('active', 'halted', 'cancelled')
         AND current_period_end IS NOT NULL
       ORDER BY created_at DESC LIMIT 1`,
    )
    .bind(user.id)
    .first<{ current_period_end: string | null }>();
  const keyExpiresAt = paidSub?.current_period_end ?? null;

  await db
    .prepare(
      `INSERT INTO api_keys (user_id, key_hash, key_prefix, name, tier, limits_json, expires_at)
       VALUES (?, ?, ?, ?, ?, ?, ?)`,
    )
    .bind(
      user.id,
      keyHash,
      keyPrefix,
      name,
      tierConfig.name,
      JSON.stringify(tierConfig.limits),
      keyExpiresAt,
    )
    .run();

  // Return the full key ONCE — never shown again
  return c.json({
    key,
    key_prefix: keyPrefix,
    name,
    tier: tierConfig.name,
    limits: tierConfig.limits,
    expires_at: keyExpiresAt,
    created_at: new Date().toISOString(),
  });
});

/**
 * DELETE /v1/dashboard/keys/:id — revoke (hard-delete) a key.
 *
 * Historically this was a soft-delete (`UPDATE revoked_at = …`) on the
 * theory that we'd want an audit trail. In practice the row just sat in
 * the dashboard with a grey "revoked" label that users read as "it
 * didn't work" and clicked again. The key_hash stops validating the
 * instant the row's marker is set anyway, so there's no correctness
 * benefit to keeping the row.
 *
 * Hard-delete now: the row vanishes from /keys, the license-validate
 * endpoint will 401 on any cached instance, and the dashboard UX
 * matches user expectation (click revoke → key gone).
 *
 * Scoped by user_id to prevent id-spoofing between accounts. If the
 * caller's user_id does not own this key (including already-deleted
 * ones), we return 404 without leaking existence info.
 */
dashboardRoutes.delete("/keys/:id", async (c) => {
  const user = c.get("user");
  const db = c.env.RECON_DB;
  const keyId = c.req.param("id");

  const result = await db
    .prepare("DELETE FROM api_keys WHERE id = ? AND user_id = ?")
    .bind(keyId, user.id)
    .run();

  if (!result.meta.changes || result.meta.changes === 0) {
    return c.json({ error: "Key not found" }, 404);
  }

  return c.json({ ok: true });
});

/**
 * GET /v1/dashboard/repos — list repos the user has registered with the
 * worker via `recon init` (server-side `max_repos` enforcement,
 * v0.2.0+).
 *
 * Mirrors the API-key-Bearer endpoint at `/v1/account/repos` but uses
 * the dashboard's session-cookie auth instead — so the dashboard JS
 * (browser session) and the CLI (Bearer token) read the same data
 * through different doors.
 *
 * Limit comes from the user's most recent non-revoked api_key's
 * `limits_json.max_repos` so the displayed N / limit always matches
 * what the worker is actually enforcing for that user. Falls back to
 * the user's `users.tier` config if no live key is found (rare —
 * happens only for users mid-rotation or just after delete-then-create).
 */
// ── GET /v1/dashboard/savings ────────────────────────────────────────────────
//
// Returns the daily token-savings series and aggregate totals for the
// dashboard "Savings" panel. Pro/Team/Enterprise only — Free tier gets a
// 200 with an upsell payload (so the dashboard renders a clean
// "upgrade to enable" card instead of an error).
//
// Range cap by tier (days, inclusive of today):
//   Free        → 0   (upsell payload, no query)
//   Pro         → 30
//   Team        → 90
//   Enterprise  → 365
//
// Hot path is a single equality+range scan on the (user_id, day) PK, so
// no extra index lookup is needed. JS aggregates the small result set
// in one pass — cheaper than a second round trip for a SUM() query.

/** Range cap by tier, in days. */
function savingsRangeDays(tier: string): number {
  switch (tier) {
    case "Pro":
      return 30;
    case "Team":
      return 90;
    case "Enterprise":
      return 365;
    default:
      return 0;
  }
}

/** UTC YYYY-MM-DD for `daysAgo` days before today. */
function utcDayString(daysAgo: number): string {
  const d = new Date();
  d.setUTCDate(d.getUTCDate() - daysAgo);
  return d.toISOString().slice(0, 10);
}

interface RollupRow {
  day: string;
  calls: number;
  response_tokens: number;
  baseline_tokens: number;
  tokens_saved: number;
  latency_micros: number;
}

dashboardRoutes.get("/savings", async (c) => {
  const user = c.get("user");
  const tier = user.tier;
  const cap = savingsRangeDays(tier);

  // Free tier: short-circuit with an upsell payload. Same JSON shape as
  // a paid response so the dashboard renders without conditional code,
  // but `daily` is empty and `upsell` is set.
  if (cap === 0) {
    return c.json({
      tier,
      range_days: 0,
      daily: [],
      totals: {
        calls: 0,
        response_tokens: 0,
        baseline_tokens: 0,
        tokens_saved: 0,
        latency_micros: 0,
      },
      upsell: {
        message:
          "Token-savings rollups are a Pro/Team feature. Upgrade your plan to start seeing aggregate savings across your sessions.",
        upgrade_url: "https://mcprecon.pages.dev/pricing",
      },
    });
  }

  // Honour an optional ?range=<days> down-shift (1..cap). Default to cap.
  const url = new URL(c.req.url);
  const requested = Number(url.searchParams.get("range"));
  const range =
    Number.isFinite(requested) && Number.isInteger(requested) && requested > 0
      ? Math.min(requested, cap)
      : cap;

  const today = utcDayString(0);
  const start = utcDayString(range - 1);
  const db = c.env.RECON_DB;

  // SUM across `repo_fingerprint` per day so the daily series is the true
  // cross-repo total, not the high-water mark of one repo. The PK
  // `(user_id, repo_fingerprint, day)` is leading-`user_id` so this is
  // still a contiguous slice — the GROUP BY just folds the per-repo rows
  // before they hit JS. For a single-repo user the result is identical
  // to v0.3.2 (one repo per day-bucket); for multi-repo users this is
  // strictly more accurate.
  const result = await db
    .prepare(
      `SELECT day,
              SUM(calls)           AS calls,
              SUM(response_tokens) AS response_tokens,
              SUM(baseline_tokens) AS baseline_tokens,
              SUM(tokens_saved)    AS tokens_saved,
              SUM(latency_micros)  AS latency_micros
       FROM usage_rollups
       WHERE user_id = ? AND day >= ? AND day <= ?
       GROUP BY day
       ORDER BY day ASC`,
    )
    .bind(user.id, start, today)
    .all<RollupRow>();

  const daily = result.results ?? [];

  // JS-side fold to compute totals. ~30..365 rows max, faster than a
  // second SUM() round-trip for our range caps.
  const totals = daily.reduce(
    (acc, r) => {
      acc.calls += r.calls;
      acc.response_tokens += r.response_tokens;
      acc.baseline_tokens += r.baseline_tokens;
      acc.tokens_saved += r.tokens_saved;
      acc.latency_micros += r.latency_micros;
      return acc;
    },
    {
      calls: 0,
      response_tokens: 0,
      baseline_tokens: 0,
      tokens_saved: 0,
      latency_micros: 0,
    },
  );

  return c.json({
    tier,
    range_days: range,
    daily,
    totals,
  });
});

dashboardRoutes.get("/repos", async (c) => {
  const user = c.get("user");
  const db = c.env.RECON_DB;

  const [reposResult, keyRow] = await Promise.all([
    db
      .prepare(
        `SELECT fingerprint, first_seen_at, last_seen_at
         FROM user_repos
         WHERE user_id = ?
         ORDER BY last_seen_at DESC`,
      )
      .bind(user.id)
      .all<{ fingerprint: string; first_seen_at: string; last_seen_at: string }>(),
    db
      .prepare(
        `SELECT tier, limits_json FROM api_keys
         WHERE user_id = ? AND revoked_at IS NULL
         ORDER BY created_at DESC LIMIT 1`,
      )
      .bind(user.id)
      .first<{ tier: string; limits_json: string }>(),
  ]);

  let limit = 1;
  let tier = user.tier;
  if (keyRow) {
    tier = keyRow.tier;
    try {
      const parsed = JSON.parse(keyRow.limits_json) as { max_repos?: number };
      if (typeof parsed.max_repos === "number" && parsed.max_repos > 0) {
        limit = parsed.max_repos;
      }
    } catch {
      // Fall through to tier config below.
    }
  }
  if (!keyRow || limit === 1) {
    // No live key, or limits_json was malformed — fall back to the
    // canonical tier config so the dashboard never under-reports.
    const cfg = getTierConfig(tier);
    if (cfg.limits?.max_repos) limit = cfg.limits.max_repos;
  }

  return c.json({
    repos: reposResult.results ?? [],
    limit,
    tier,
  });
});

/**
 * DELETE /v1/dashboard/repos/:fingerprint — release a repo slot from the
 * dashboard. Same atomicity guarantees as the Bearer-auth endpoint:
 * row scoped to user_id so one user can't delete another's slot.
 */
dashboardRoutes.delete("/repos/:fingerprint", async (c) => {
  const user = c.get("user");
  const db = c.env.RECON_DB;
  const fp = c.req.param("fingerprint");
  if (!/^[0-9a-f]{64}$/.test(fp)) {
    return c.json({ error: "fingerprint must be 64-char lowercase hex (SHA-256)" }, 400);
  }

  const result = await db
    .prepare("DELETE FROM user_repos WHERE user_id = ? AND fingerprint = ?")
    .bind(user.id, fp)
    .run();

  if (!result.meta.changes || result.meta.changes === 0) {
    return c.json({ error: "fingerprint not registered" }, 404);
  }
  return new Response(null, { status: 204 });
});

/**
 * DELETE /v1/dashboard/account — permanently delete the user's account and
 * every row that references it.
 *
 * Rows affected per delete:
 *   * users  — the row itself
 *   * api_keys, sessions, payments, subscriptions — ON DELETE CASCADE from
 *     users(id) in migration 0001, so one DELETE on users drops all four.
 *   * payment_events — no FK (keyed by razorpay_payment_id). We clean
 *     these up manually by joining on razorpay_order_id, so a deleted
 *     user's webhook-delivery history doesn't leak the old payment IDs
 *     into future idempotency checks.
 *
 * Before deleting we call Razorpay `cancelSubscription` with
 * cancel_at_cycle_end=0 for every live subscription, so Razorpay itself
 * stops charging the user. We swallow per-sub errors so a single stale
 * Razorpay record (already cancelled, already terminated) doesn't block
 * the whole account deletion — the delete has to succeed even if
 * Razorpay's side is out of sync.
 *
 * Session cookie: the user's session row is cascaded away along with the
 * user, so the next request with that cookie 401s naturally. We don't
 * bother clearing the cookie here — the frontend is expected to
 * window.location to /login immediately on 200 anyway.
 */
dashboardRoutes.delete("/account", async (c) => {
  const user = c.get("user");
  const db = c.env.RECON_DB;

  // 1. Cancel any live Razorpay subscriptions immediately. No
  // cancel_at_cycle_end for account deletion — we don't want the user to
  // get one more charge after "delete my account".
  const { results: liveSubs = [] } = await db
    .prepare(
      `SELECT razorpay_subscription_id
       FROM subscriptions
       WHERE user_id = ?
         AND razorpay_subscription_id IS NOT NULL
         AND status IN ('created','authenticated','active','pending','halted')`,
    )
    .bind(user.id)
    .all<{ razorpay_subscription_id: string }>();

  for (const row of liveSubs) {
    try {
      await cancelSubscription(
        c.env.RAZORPAY_KEY_ID,
        c.env.RAZORPAY_KEY_SECRET,
        row.razorpay_subscription_id,
        false, // immediate — not cycle-end
      );
    } catch (e) {
      console.warn(
        `account delete: Razorpay cancel failed for ${row.razorpay_subscription_id}:`,
        e,
      );
      // continue — Razorpay might already have it cancelled; don't block delete
    }
  }

  // 2. Wipe payment_events tied to this user's payments (no FK cascade).
  // 3. Delete the user row — cascades everywhere else.
  // One batch so the whole thing is atomic.
  await db.batch([
    db
      .prepare(
        `DELETE FROM payment_events
         WHERE razorpay_order_id IN (
           SELECT razorpay_order_id FROM payments WHERE user_id = ?
         )`,
      )
      .bind(user.id),
    db.prepare("DELETE FROM users WHERE id = ?").bind(user.id),
  ]);

  return c.json({
    ok: true,
    user_id: user.id,
    subscriptions_cancelled: liveSubs.length,
  });
});
