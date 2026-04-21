import type { Context, Next } from "hono";
import { getCookie } from "hono/cookie";
import { sha256Hex } from "../lib/crypto";
import type { AuthUser, Env } from "../types";

/** Require authentication via Bearer token header or __Host-session cookie. */
export async function requireAuth(
  c: Context<{ Bindings: Env; Variables: { user: AuthUser } }>,
  next: Next,
): Promise<Response | void> {
  // Try Bearer token first (dashboard JS uses this), fall back to cookie
  let token = getCookie(c, "__Host-session");
  const authHeader = c.req.header("Authorization");
  if (authHeader?.startsWith("Bearer ")) {
    token = authHeader.slice(7).trim();
  }
  if (!token) {
    return c.json({ error: "Not authenticated" }, 401);
  }

  const tokenHash = await sha256Hex(token);
  const db = c.env.RECON_DB;

  // Expiry check is pushed to the DB — eliminates the separate DELETE round-trip
  // on expired sessions (SQLite handles the filter; expired rows stay until vacuum).
  const row = await db
    .prepare(
      `SELECT s.user_id,
              u.id, u.github_username, u.email, u.avatar_url, u.tier
       FROM sessions s
       JOIN users u ON s.user_id = u.id
       WHERE s.token_hash = ? AND s.expires_at > datetime('now')`,
    )
    .bind(tokenHash)
    .first();

  if (!row) {
    return c.json({ error: "Invalid or expired session" }, 401);
  }

  c.set("user", {
    id: row.user_id as string,
    github_username: row.github_username as string,
    email: (row.email as string) ?? null,
    avatar_url: (row.avatar_url as string) ?? null,
    tier: row.tier as string,
  });

  await next();
}
