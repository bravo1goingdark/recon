/**
 * POST /v1/license/validate
 *
 * Wire-compatible with the Rust CLI at crates/recon-server/src/license.rs.
 * The CLI sends: Authorization: Bearer {key} AND body {"key": "..."}.
 * We accept the key from either source.
 *
 * Every valid response is HMAC-SHA256 signed over the canonical payload:
 *   "{tier}:{max_repos}:{max_files}:{max_loc}:{expires_at}"
 * using the LICENSE_HMAC_SECRET environment variable.  The CLI verifies this
 * signature before trusting the cached response, preventing local tampering.
 */

import { Hono } from "hono";
import { sha256Hex, hmacSha256Hex } from "../lib/crypto";
import { getTierConfig } from "../lib/tiers";
import { clientIp, rateLimit } from "../middleware/ratelimit";
import type { Env, LicenseValidateResponse } from "../types";

export const licenseRoutes = new Hono<{ Bindings: Env }>();

/**
 * Per-key-prefix rate limit guards against brute-force validation of random
 * API keys. Key prefix is the first 14 chars — unique per issued key, so a
 * single stolen-prefix attacker can't exhaust the bucket for every user.
 * Unauthenticated (no Authorization header / body key) requests fall back
 * to the caller's IP so the limiter can't be bypassed by omitting the key.
 */
async function licenseRateKey(
  c: Parameters<Parameters<typeof rateLimit>[1]>[0],
): Promise<string> {
  const auth = c.req.header("Authorization")?.replace(/^Bearer\s+/i, "").trim();
  if (auth && auth.length >= 14) return `key:${auth.slice(0, 14)}`;
  return `ip:${clientIp(c)}`;
}

licenseRoutes.use("*", rateLimit("RL_LICENSE", licenseRateKey, 60));

licenseRoutes.post("/validate", async (c) => {
  // Extract key from Authorization header or body
  let key: string | undefined;

  const authHeader = c.req.header("Authorization");
  if (authHeader) {
    key = authHeader.replace(/^Bearer\s+/i, "").trim();
  }

  // Also check body (CLI sends both header and body)
  try {
    const body = await c.req.json<{ key?: string }>();
    if (body.key && typeof body.key === "string") {
      key = body.key;
    }
  } catch {
    // Body may be empty or malformed — ignore
  }

  if (!key) {
    return c.json({ error: "No API key provided", valid: false }, 401);
  }

  const keyHash = await sha256Hex(key);
  const db = c.env.RECON_DB;

  const row = await db
    .prepare(
      `SELECT ak.tier, ak.limits_json, ak.expires_at, ak.revoked_at
       FROM api_keys ak
       WHERE ak.key_hash = ?`,
    )
    .bind(keyHash)
    .first();

  if (!row) {
    return c.json({ error: "Invalid API key", valid: false }, 401);
  }

  if (row.revoked_at) {
    return c.json({ error: "API key has been revoked", valid: false }, 401);
  }

  // Parse expiry
  let expiresAtUnix = 0;
  if (row.expires_at) {
    expiresAtUnix = Math.floor(
      new Date(row.expires_at as string).getTime() / 1000,
    );
    if (Date.now() / 1000 > expiresAtUnix) {
      return c.json({ error: "API key has expired — renew at recon.dev", valid: false }, 401);
    }
  }

  const limits = JSON.parse(row.limits_json as string) as {
    max_repos: number;
    max_files: number;
    max_loc: number;
  };
  const tier = row.tier as string;

  // Sign the canonical payload so the CLI can detect local tampering.
  const payload = `${tier}:${limits.max_repos}:${limits.max_files}:${limits.max_loc}:${expiresAtUnix}`;
  const signature = await hmacSha256Hex(c.env.LICENSE_HMAC_SECRET, payload);

  return c.json({
    valid: true,
    tier,
    limits,
    expires_at: expiresAtUnix,
    message: `${tier} plan active${row.expires_at ? " until " + (row.expires_at as string).split("T")[0] : ""}`,
    signature,
  } satisfies LicenseValidateResponse);
});
