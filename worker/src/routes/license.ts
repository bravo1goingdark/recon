/**
 * POST /v1/license/validate
 *
 * Wire-compatible with the Rust CLI at crates/recon-server/src/license.rs.
 * The CLI sends: Authorization: Bearer {key} AND body {"key": "..."}.
 * We accept the key from either source.
 */

import { Hono } from "hono";
import { sha256Hex } from "../lib/crypto";
import { getTierConfig } from "../lib/tiers";
import type { Env, LicenseValidateResponse } from "../types";

export const licenseRoutes = new Hono<{ Bindings: Env }>();

const FREE_RESPONSE: LicenseValidateResponse = {
  valid: false,
  tier: "Free",
  limits: getTierConfig("Free").limits,
  expires_at: 0,
  message: "No API key provided",
};

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
    return c.json(FREE_RESPONSE);
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
    return c.json({
      ...FREE_RESPONSE,
      message: "Invalid API key",
    });
  }

  if (row.revoked_at) {
    return c.json({
      ...FREE_RESPONSE,
      message: "API key has been revoked",
    });
  }

  // Check expiry
  let expiresAtUnix = 0;
  if (row.expires_at) {
    expiresAtUnix = Math.floor(
      new Date(row.expires_at as string).getTime() / 1000,
    );
    if (Date.now() / 1000 > expiresAtUnix) {
      return c.json({
        valid: false,
        tier: "Free",
        limits: getTierConfig("Free").limits,
        expires_at: expiresAtUnix,
        message: "API key has expired — renew at recon.dev",
      } satisfies LicenseValidateResponse);
    }
  }

  const limits = JSON.parse(row.limits_json as string);
  const tier = row.tier as string;

  return c.json({
    valid: true,
    tier,
    limits,
    expires_at: expiresAtUnix,
    message: `${tier} plan active${row.expires_at ? " until " + (row.expires_at as string).split("T")[0] : ""}`,
  } satisfies LicenseValidateResponse);
});
