/**
 * /v1/license/validate contract tests.
 *
 * This endpoint IS the wire protocol between the Rust CLI and the server.
 * The signature scheme is locked in `crates/recon-server/src/license.rs`
 * (Rust side) and `src/lib/crypto.ts` (server side). A drift here silently
 * breaks every deployed CLI. The test pins both:
 *   - canonical payload format: `{tier}:{max_repos}:{max_files}:{max_loc}:{expires_at}`
 *   - HMAC-SHA256 hex output over that payload with LICENSE_HMAC_SECRET
 */

import { beforeEach, describe, expect, it } from "vitest";
import { env, getJson, resetDb } from "./setup";

/** Compute the expected HMAC the same way the Worker does. */
async function hmacHex(secret: string, payload: string): Promise<string> {
  const enc = new TextEncoder();
  const key = await crypto.subtle.importKey(
    "raw",
    enc.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const sig = await crypto.subtle.sign("HMAC", key, enc.encode(payload));
  return Array.from(new Uint8Array(sig))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/** SHA-256 hex — matches `sha256Hex` in the Worker. */
async function sha256Hex(input: string): Promise<string> {
  const buf = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(input),
  );
  return Array.from(new Uint8Array(buf))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

async function seedKey(opts: {
  userId: string;
  key: string;
  tier: string;
  maxRepos: number;
  maxFiles: number;
  maxLoc: number;
  revoked?: boolean;
  expiresAt?: string | null;
}): Promise<void> {
  const db = (env as { RECON_DB: D1Database }).RECON_DB;
  const keyHash = await sha256Hex(opts.key);
  await db
    .prepare(
      `INSERT INTO users (id, github_id, github_username, tier)
       VALUES (?, ?, 'carol', ?)`,
    )
    .bind(opts.userId, 7, opts.tier)
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
      opts.tier,
      JSON.stringify({
        max_repos: opts.maxRepos,
        max_files: opts.maxFiles,
        max_loc: opts.maxLoc,
      }),
      opts.expiresAt ?? null,
      opts.revoked ? new Date().toISOString() : null,
    )
    .run();
}

describe("POST /v1/license/validate", () => {
  beforeEach(async () => {
    await resetDb();
  });

  it("rejects missing key with 401", async () => {
    const res = await getJson("/v1/license/validate", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({}),
    });
    expect(res.status).toBe(401);
    expect(res.body).toMatchObject({ valid: false });
  });

  it("rejects unknown key with 401", async () => {
    const res = await getJson("/v1/license/validate", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: "Bearer sk-recon-notreal",
      },
      body: JSON.stringify({}),
    });
    expect(res.status).toBe(401);
    expect(res.body).toMatchObject({ valid: false });
  });

  it("rejects revoked key with 401", async () => {
    await seedKey({
      userId: "user_rev",
      key: "sk-recon-revoked1",
      tier: "Pro",
      maxRepos: 10,
      maxFiles: 50000,
      maxLoc: 2_000_000,
      revoked: true,
    });
    const res = await getJson("/v1/license/validate", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: "Bearer sk-recon-revoked1",
      },
      body: JSON.stringify({ key: "sk-recon-revoked1" }),
    });
    expect(res.status).toBe(401);
    expect(res.body).toMatchObject({ valid: false });
  });

  it("returns signed payload for a valid key — matches CLI verification", async () => {
    const tier = "Pro";
    const maxRepos = 10;
    const maxFiles = 50_000;
    const maxLoc = 2_000_000;
    await seedKey({
      userId: "user_ok",
      key: "sk-recon-validkey1",
      tier,
      maxRepos,
      maxFiles,
      maxLoc,
    });

    const res = await getJson("/v1/license/validate", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: "Bearer sk-recon-validkey1",
      },
      body: JSON.stringify({}),
    });

    expect(res.status).toBe(200);
    type LicenseBody = {
      valid: boolean;
      tier: string;
      limits: { max_repos: number; max_files: number; max_loc: number };
      expires_at: number;
      signature: string;
    };
    const body = res.body as LicenseBody;
    expect(body.valid).toBe(true);
    expect(body.tier).toBe(tier);
    expect(body.limits).toEqual({
      max_repos: maxRepos,
      max_files: maxFiles,
      max_loc: maxLoc,
    });
    expect(body.expires_at).toBe(0); // no expiry configured
    // Contract: signature == HMAC-SHA256(LICENSE_HMAC_SECRET, payload)
    // where payload is exactly `{tier}:{max_repos}:{max_files}:{max_loc}:{expires_at}`.
    // The Rust CLI does the same computation in validate_cached_response.
    const payload = `${tier}:${maxRepos}:${maxFiles}:${maxLoc}:0`;
    const expected = await hmacHex("test-license-hmac-secret", payload);
    expect(body.signature).toBe(expected);
  });
});
