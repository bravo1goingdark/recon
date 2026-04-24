import { Hono } from "hono";
import { sha256Hex, generateApiKey } from "../lib/crypto";
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

/** POST /v1/dashboard/keys — generate a new API key. */
dashboardRoutes.post("/keys", async (c) => {
  const user = c.get("user");
  const db = c.env.RECON_DB;

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

  await db
    .prepare(
      `INSERT INTO api_keys (user_id, key_hash, key_prefix, name, tier, limits_json)
       VALUES (?, ?, ?, ?, ?, ?)`,
    )
    .bind(
      user.id,
      keyHash,
      keyPrefix,
      name,
      tierConfig.name,
      JSON.stringify(tierConfig.limits),
    )
    .run();

  // Return the full key ONCE — never shown again
  return c.json({
    key,
    key_prefix: keyPrefix,
    name,
    tier: tierConfig.name,
    limits: tierConfig.limits,
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
