import { sha256Hex, generateSessionToken } from "./crypto";

const SESSION_TTL_DAYS = 7;

/** Create a session in D1, return the raw token (unhashed). */
export async function createSession(
  db: D1Database,
  userId: string,
): Promise<string> {
  const token = generateSessionToken();
  const tokenHash = await sha256Hex(token);
  const expiresAt = new Date(
    Date.now() + SESSION_TTL_DAYS * 86_400_000,
  ).toISOString();

  await db
    .prepare(
      "INSERT INTO sessions (user_id, token_hash, expires_at) VALUES (?, ?, ?)",
    )
    .bind(userId, tokenHash, expiresAt)
    .run();

  return token;
}

/** Destroy a session by raw token. */
export async function destroySession(
  db: D1Database,
  token: string,
): Promise<void> {
  const tokenHash = await sha256Hex(token);
  await db
    .prepare("DELETE FROM sessions WHERE token_hash = ?")
    .bind(tokenHash)
    .run();
}

/** Set-Cookie header value for the session. */
export function sessionCookie(
  token: string,
  maxAge: number = SESSION_TTL_DAYS * 86_400,
): string {
  return `__Host-session=${token}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=${maxAge}`;
}

/** Set-Cookie header value that clears the session. */
export function clearSessionCookie(): string {
  return "__Host-session=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0";
}
