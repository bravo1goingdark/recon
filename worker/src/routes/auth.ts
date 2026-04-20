import { Hono } from "hono";
import { getCookie } from "hono/cookie";
import { exchangeCodeForToken, fetchGitHubUser } from "../lib/github";
import { sha256Hex, generateApiKey, generateSessionToken } from "../lib/crypto";
import { createSession, destroySession, sessionCookie, clearSessionCookie } from "../lib/session";
import { getTierConfig } from "../lib/tiers";
import { requireAuth } from "../middleware/auth";
import type { AuthUser, Env } from "../types";

export const authRoutes = new Hono<{
  Bindings: Env;
  Variables: { user: AuthUser };
}>();

/** GET /v1/auth/github — redirect to GitHub OAuth. */
authRoutes.get("/github", (c) => {
  // Build callback URL matching the request prefix (/api/v1 or /v1)
  const reqUrl = new URL(c.req.url);
  const callbackPath = reqUrl.pathname.replace(/\/github$/, "/github/callback");
  const redirectUri = reqUrl.origin + callbackPath;
  const state = generateSessionToken().slice(0, 32); // CSRF token

  const url = new URL("https://github.com/login/oauth/authorize");
  url.searchParams.set("client_id", c.env.GITHUB_CLIENT_ID);
  url.searchParams.set("redirect_uri", redirectUri);
  url.searchParams.set("scope", "read:user user:email");
  url.searchParams.set("state", state);

  return c.redirect(url.toString());
});

/** GET /v1/auth/github/callback — handle OAuth callback. */
authRoutes.get("/github/callback", async (c) => {
  const code = c.req.query("code");
  if (!code) {
    return c.json({ error: "Missing code parameter" }, 400);
  }

  // Must match exactly what was sent in the authorize request
  const reqUrl = new URL(c.req.url);
  const redirectUri = reqUrl.origin + reqUrl.pathname;

  // Exchange code for access token
  const accessToken = await exchangeCodeForToken(
    code,
    c.env.GITHUB_CLIENT_ID,
    c.env.GITHUB_CLIENT_SECRET,
    redirectUri,
  );

  // Fetch GitHub user profile
  const ghUser = await fetchGitHubUser(accessToken);

  const db = c.env.RECON_DB;

  // Upsert user
  await db
    .prepare(
      `INSERT INTO users (github_id, github_username, email, avatar_url)
       VALUES (?, ?, ?, ?)
       ON CONFLICT(github_id) DO UPDATE SET
         github_username = excluded.github_username,
         email = excluded.email,
         avatar_url = excluded.avatar_url,
         updated_at = datetime('now')`,
    )
    .bind(ghUser.id, ghUser.login, ghUser.email, ghUser.avatar_url)
    .run();

  // Get user row (need the id)
  const user = await db
    .prepare("SELECT id, tier FROM users WHERE github_id = ?")
    .bind(ghUser.id)
    .first();

  if (!user) {
    return c.json({ error: "Failed to create user" }, 500);
  }

  const userId = user.id as string;
  const userTier = user.tier as string;

  // Auto-generate a Free API key if user has none
  const keyCount = await db
    .prepare(
      "SELECT COUNT(*) as cnt FROM api_keys WHERE user_id = ? AND revoked_at IS NULL",
    )
    .bind(userId)
    .first();

  if (!keyCount || (keyCount.cnt as number) === 0) {
    const newKey = generateApiKey();
    const keyHash = await sha256Hex(newKey);
    const keyPrefix = newKey.slice(0, 14); // sk-recon-xxxx
    const tierConfig = getTierConfig(userTier);

    await db
      .prepare(
        `INSERT INTO api_keys (user_id, key_hash, key_prefix, name, tier, limits_json)
         VALUES (?, ?, ?, 'Default', ?, ?)`,
      )
      .bind(
        userId,
        keyHash,
        keyPrefix,
        tierConfig.name,
        JSON.stringify(tierConfig.limits),
      )
      .run();
  }

  // Create session
  const token = await createSession(db, userId);

  // Redirect to dashboard with session cookie
  const frontendUrl = c.env.FRONTEND_URL || "https://recon.dev";
  return new Response(null, {
    status: 302,
    headers: {
      Location: `${frontendUrl}/dashboard`,
      "Set-Cookie": sessionCookie(token),
    },
  });
});

/** GET /v1/auth/me — return current user. */
authRoutes.get("/me", requireAuth, (c) => {
  const user = c.get("user");
  return c.json(user);
});

/** POST /v1/auth/logout — clear session. */
authRoutes.post("/logout", async (c) => {
  const token = getCookie(c, "__Host-session");
  if (token) {
    await destroySession(c.env.RECON_DB, token);
  }

  return new Response(JSON.stringify({ ok: true }), {
    status: 200,
    headers: {
      "Content-Type": "application/json",
      "Set-Cookie": clearSessionCookie(),
    },
  });
});
