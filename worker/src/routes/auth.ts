import { Hono } from "hono";
import type { Context } from "hono";
import { getCookie } from "hono/cookie";
import { exchangeCodeForToken, fetchGitHubUser } from "../lib/github";
import { sha256Hex, generateApiKey, generateSessionToken, timingSafeEqual } from "../lib/crypto";
import { createSession, destroySession, sessionCookie, clearSessionCookie } from "../lib/session";
import { getTierConfig } from "../lib/tiers";
import { requireAuth } from "../middleware/auth";
import type { AuthUser, Env } from "../types";

export const authRoutes = new Hono<{
  Bindings: Env;
  Variables: { user: AuthUser };
}>();

type AuthContext = Context<{
  Bindings: Env;
  Variables: { user: AuthUser };
}>;

/**
 * Compute the browser-visible origin for this request.
 *
 * The Worker can be reached two ways:
 *   - Directly at recon-api.kumarashutosh34169.workers.dev (dev / curl / CI).
 *   - Through the Pages proxy at mcprecon.pages.dev/api/… (production UX).
 *
 * In the proxied case, `c.req.url` shows the Worker's own origin — useless
 * for the OAuth redirect_uri, because GitHub validates against the origin
 * the *browser* will hit, not the origin the Worker receives the request
 * on. The Pages Function forwards the original host in X-Forwarded-Host,
 * letting us rebuild the URL the browser actually sees.
 *
 * Falls back to the request URL's own origin when the header is absent
 * (direct-hit path) so local dev / curl smoke tests still work.
 */
const OAUTH_STATE_COOKIE = "__Host-oauth-state";
const OAUTH_STATE_MAX_AGE_SECS = 600;

function allowedOriginHosts(c: AuthContext): Set<string> {
  const hosts = new Set<string>();
  for (const raw of c.env.ALLOWED_ORIGINS.split(",")) {
    const trimmed = raw.trim();
    if (!trimmed) continue;
    try {
      hosts.add(new URL(trimmed).host);
    } catch {
      // Ignore malformed config entries; CORS will ignore them too.
    }
  }
  if (c.env.FRONTEND_URL) {
    try {
      hosts.add(new URL(c.env.FRONTEND_URL).host);
    } catch {
      // Optional safety net only.
    }
  }
  return hosts;
}

function browserOrigin(c: AuthContext): string {
  const fwdHost = c.req.header("x-forwarded-host");
  const fwdProto = c.req.header("x-forwarded-proto") || "https";
  if (fwdHost && allowedOriginHosts(c).has(fwdHost)) {
    const proto = fwdProto === "http" ? "http" : "https";
    return `${proto}://${fwdHost}`;
  }
  return new URL(c.req.url).origin;
}

function oauthStateCookie(state: string): string {
  return `${OAUTH_STATE_COOKIE}=${state}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=${OAUTH_STATE_MAX_AGE_SECS}`;
}

function clearOauthStateCookie(): string {
  return `${OAUTH_STATE_COOKIE}=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0`;
}

/** GET /v1/auth/github — redirect to GitHub OAuth. */
authRoutes.get("/github", (c) => {
  // Build callback URL matching the request prefix (/api/v1 or /v1) and
  // the browser-visible origin (NOT the Worker's own origin when proxied
  // via the Pages Function — GitHub validates redirect_uri against what
  // the browser will hit, not what the Worker sees).
  const reqUrl = new URL(c.req.url);
  const callbackPath = reqUrl.pathname.replace(/\/github$/, "/github/callback");
  const redirectUri = browserOrigin(c) + callbackPath;
  const state = generateSessionToken().slice(0, 32); // CSRF token

  const url = new URL("https://github.com/login/oauth/authorize");
  url.searchParams.set("client_id", c.env.GITHUB_CLIENT_ID);
  url.searchParams.set("redirect_uri", redirectUri);
  url.searchParams.set("scope", "read:user user:email");
  url.searchParams.set("state", state);

  return new Response(null, {
    status: 302,
    headers: {
      Location: url.toString(),
      "Set-Cookie": oauthStateCookie(state),
    },
  });
});

/** GET /v1/auth/github/callback — handle OAuth callback. */
authRoutes.get("/github/callback", async (c) => {
  const code = c.req.query("code");
  if (!code) {
    return c.json({ error: "Missing code parameter" }, 400);
  }
  const state = c.req.query("state");
  const expectedState = getCookie(c, OAUTH_STATE_COOKIE);
  if (!state || !expectedState || !timingSafeEqual(state, expectedState)) {
    return new Response(JSON.stringify({ error: "Invalid OAuth state" }), {
      status: 400,
      headers: {
        "Content-Type": "application/json",
        "Set-Cookie": clearOauthStateCookie(),
      },
    });
  }

  // Must match exactly what was sent in the authorize request — that
  // means the browser-visible origin (Pages), not the Worker origin.
  const reqUrl = new URL(c.req.url);
  const redirectUri = browserOrigin(c) + reqUrl.pathname;

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

  // Upsert user and return id + tier in a single round-trip (SQLite RETURNING).
  const user = await db
    .prepare(
      `INSERT INTO users (github_id, github_username, email, avatar_url)
       VALUES (?, ?, ?, ?)
       ON CONFLICT(github_id) DO UPDATE SET
         github_username = excluded.github_username,
         email = excluded.email,
         avatar_url = excluded.avatar_url,
         updated_at = datetime('now')
       RETURNING id, tier`,
    )
    .bind(ghUser.id, ghUser.login, ghUser.email, ghUser.avatar_url)
    .first();

  if (!user) {
    return c.json({ error: "Failed to create user" }, 500);
  }

  const userId = user.id as string;
  const userTier = user.tier as string;

  // Auto-generate a Free API key if user has none.
  // SELECT 1 LIMIT 1 is cheaper than COUNT(*) — we only need existence.
  const hasKey = await db
    .prepare(
      "SELECT 1 FROM api_keys WHERE user_id = ? AND revoked_at IS NULL LIMIT 1",
    )
    .bind(userId)
    .first();

  if (!hasKey) {
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

  // Create session and hand the token to the browser via an HttpOnly,
  // Secure, SameSite=Lax cookie. JS cannot read it → immune to XSS token
  // exfiltration. `__Host-` prefix enforces Secure + Path=/ + no Domain.
  // SameSite=Lax allows the cookie on GitHub's top-level redirect back to
  // us but blocks cross-site POSTs (CSRF protection).
  const token = await createSession(db, userId);
  const frontendUrl = browserOrigin(c);
  const headers = new Headers({ Location: `${frontendUrl}/dashboard` });
  headers.append("Set-Cookie", sessionCookie(token));
  headers.append("Set-Cookie", clearOauthStateCookie());
  return new Response(null, {
    status: 302,
    headers,
  });
});

/** GET /v1/auth/me — return current user. */
authRoutes.get("/me", requireAuth, (c) => {
  const user = c.get("user");
  return c.json(user);
});

/** POST /v1/auth/logout — destroy session. */
authRoutes.post("/logout", requireAuth, async (c) => {
  // The token arrives via either the __Host-session cookie (browser) or a
  // Bearer header (legacy callers). Destroy whichever we find, and always
  // emit a clearing Set-Cookie so the browser drops the HttpOnly cookie.
  const authHeader = c.req.header("Authorization");
  let token: string | undefined;
  if (authHeader?.startsWith("Bearer ")) {
    token = authHeader.slice(7).trim();
  } else {
    token = getCookie(c, "__Host-session");
  }
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
