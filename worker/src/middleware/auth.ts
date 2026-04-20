import type { Context, Next } from "hono";
import { getCookie } from "hono/cookie";
import { sha256Hex } from "../lib/crypto";
import type { AuthUser, Env } from "../types";

/** Require authenticated session via __Host-session cookie. */
export async function requireAuth(
  c: Context<{ Bindings: Env; Variables: { user: AuthUser } }>,
  next: Next,
): Promise<Response | void> {
  const token = getCookie(c, "__Host-session");
  if (!token) {
    return c.json({ error: "Not authenticated" }, 401);
  }

  const tokenHash = await sha256Hex(token);
  const db = c.env.RECON_DB;

  const row = await db
    .prepare(
      `SELECT s.user_id, s.expires_at,
              u.id, u.github_username, u.email, u.avatar_url, u.tier
       FROM sessions s
       JOIN users u ON s.user_id = u.id
       WHERE s.token_hash = ?`,
    )
    .bind(tokenHash)
    .first();

  if (!row) {
    return c.json({ error: "Invalid session" }, 401);
  }

  if (new Date(row.expires_at as string) < new Date()) {
    // Clean up expired session
    await db
      .prepare("DELETE FROM sessions WHERE token_hash = ?")
      .bind(tokenHash)
      .run();
    return c.json({ error: "Session expired" }, 401);
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
