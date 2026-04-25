/**
 * API-key auth middleware for endpoints called by the CLI binary.
 *
 * Background: `requireAuth` (auth.ts) is session-based and serves the
 * browser dashboard. The CLI doesn't have a session — it ships a Bearer
 * `sk-recon-…` API key, validated against `api_keys.key_hash`. Mirroring
 * the lookup in every route would duplicate the revocation / expiry
 * checks that already live in `routes/license.ts`; this middleware
 * centralises them.
 *
 * Attaches both the user row and the api-key row to the context so
 * handlers can read tier limits without a second DB round-trip.
 */

import type { Context, Next } from "hono";
import { sha256Hex } from "../lib/crypto";
import type { ApiKeyRow, AuthUser, Env } from "../types";

/** Variables the middleware sets on the request context. */
export interface ApiKeyVars {
  /** The authenticated user (mirrors `requireAuth`'s shape so routes can be polymorphic). */
  user: AuthUser;
  /** The api_keys row that authorised this request — includes tier + limits_json. */
  apiKey: ApiKeyRow;
}

/**
 * Require a valid `Authorization: Bearer <api-key>` header.
 *
 * Rejects with 401 on:
 *   - missing / malformed header
 *   - unknown key
 *   - revoked key
 *   - expired key (`expires_at < now`)
 *
 * On success, attaches `user` and `apiKey` to `c.set(...)`.
 */
export async function requireApiKey(
  c: Context<{ Bindings: Env; Variables: ApiKeyVars }>,
  next: Next,
): Promise<Response | void> {
  const authHeader = c.req.header("Authorization");
  if (!authHeader?.startsWith("Bearer ")) {
    return c.json({ error: "Bearer API key required" }, 401);
  }
  const key = authHeader.slice(7).trim();
  if (!key) {
    return c.json({ error: "Bearer API key required" }, 401);
  }

  const keyHash = await sha256Hex(key);
  const db = c.env.RECON_DB;

  const row = await db
    .prepare(
      `SELECT ak.id, ak.user_id, ak.key_hash, ak.key_prefix, ak.name,
              ak.tier, ak.limits_json, ak.expires_at, ak.created_at, ak.revoked_at,
              u.github_username, u.email, u.avatar_url
       FROM api_keys ak
       JOIN users u ON ak.user_id = u.id
       WHERE ak.key_hash = ?`,
    )
    .bind(keyHash)
    .first<ApiKeyRow & {
      github_username: string;
      email: string | null;
      avatar_url: string | null;
    }>();

  if (!row) {
    return c.json({ error: "Invalid API key" }, 401);
  }
  if (row.revoked_at) {
    return c.json({ error: "API key has been revoked" }, 401);
  }
  if (row.expires_at) {
    const expUnix = Math.floor(new Date(row.expires_at).getTime() / 1000);
    if (Date.now() / 1000 > expUnix) {
      return c.json({ error: "API key has expired — renew at recon.dev" }, 401);
    }
  }

  c.set("user", {
    id: row.user_id,
    github_username: row.github_username,
    email: row.email ?? null,
    avatar_url: row.avatar_url ?? null,
    tier: row.tier,
  });
  c.set("apiKey", {
    id: row.id,
    user_id: row.user_id,
    key_hash: row.key_hash,
    key_prefix: row.key_prefix,
    name: row.name,
    tier: row.tier,
    limits_json: row.limits_json,
    expires_at: row.expires_at,
    created_at: row.created_at,
    revoked_at: row.revoked_at,
  });

  await next();
}

/**
 * Build a rate-limit key for API-key-authed endpoints.
 *
 * Uses the key prefix (first 14 chars) so a single key's bucket is
 * shared across requests but distinct from other keys. Falls back to
 * the IP when the auth header is missing — paired with `requireApiKey`
 * the missing-header case is rejected anyway, but the fallback keeps
 * unauthenticated probes inside the limiter.
 */
export function apiKeyRateKey(c: Context): string {
  const auth = c.req.header("Authorization")?.replace(/^Bearer\s+/i, "").trim();
  if (auth && auth.length >= 14) return `key:${auth.slice(0, 14)}`;
  const ip =
    c.req.header("CF-Connecting-IP") ||
    c.req.header("X-Real-IP") ||
    (c.req.header("X-Forwarded-For") || "").split(",")[0].trim() ||
    "unknown";
  return `ip:${ip}`;
}
