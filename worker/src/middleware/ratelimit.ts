/**
 * Rate-limit middleware built on Cloudflare's Rate Limiting API bindings.
 *
 * Each named binding in `wrangler.toml` (`[[unsafe.bindings]]` with
 * `type = "ratelimit"`) maps to one logical bucket with a period + limit.
 * The middleware looks up the caller's key (IP, user ID, or API-key hash,
 * selected by the `keyFrom` function), calls `.limit({ key })`, and emits a
 * 429 with `Retry-After` when the bucket is exhausted.
 *
 * Missing buckets fail open only for local/test requests. Production must
 * fail closed so a bad deploy cannot silently run without rate limits.
 */

import type { Context, MiddlewareHandler } from "hono";
import type { Env } from "../types";

/**
 * The subset of the Rate Limiting API we use. Cloudflare's types live in
 * `@cloudflare/workers-types` under a non-stable path; we inline the shape.
 */
interface RateLimitBinding {
  limit(options: { key: string }): Promise<{ success: boolean }>;
}

/**
 * Look up a named binding on `env` and coerce to our narrow shape.
 * Returns `null` when the binding is absent.
 */
function lookupBinding(env: Env, name: string): RateLimitBinding | null {
  const raw = (env as unknown as Record<string, unknown>)[name];
  if (raw && typeof (raw as RateLimitBinding).limit === "function") {
    return raw as RateLimitBinding;
  }
  return null;
}

function isLocalOrTestRequest(c: Pick<Context, "req">): boolean {
  const hostname = new URL(c.req.url).hostname.toLowerCase();
  return (
    hostname === "localhost" ||
    hostname === "127.0.0.1" ||
    hostname === "::1" ||
    hostname.endsWith(".local")
  );
}

/**
 * Generic over route-local `Variables` so handlers that have already
 * attached e.g. `{ user }` can still read it from the rate-limit key
 * function without fighting types.
 */
type AnyEnv = {
  Bindings: Env;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  Variables?: Record<string, any>;
};

/**
 * Build a middleware that enforces the given rate-limit bucket.
 *
 * @param bindingName  Name of the binding in wrangler.toml (e.g. `"RL_CHECKOUT"`).
 * @param keyFrom      Produces the bucket key for this request. IP and user-ID
 *                     are the common cases; `keyFrom` is async to allow DB
 *                     lookups (e.g. hashing an Authorization header).
 * @param retryAfter   Seconds to advertise in the 429 `Retry-After` header.
 *                     Should match the binding's configured period.
 */
export function rateLimit<E extends AnyEnv = { Bindings: Env }>(
  bindingName: string,
  keyFrom: (c: Context<E>) => string | Promise<string>,
  retryAfter: number,
): MiddlewareHandler<E> {
  let warnedMissing = false;
  return async (c, next) => {
    const bucket = lookupBinding(c.env, bindingName);
    if (!bucket) {
      if (isLocalOrTestRequest(c)) {
        if (!warnedMissing) {
          warnedMissing = true;
          console.warn(
            `rate-limit binding ${bindingName} missing — failing open (local/test only)`,
          );
        }
        return next();
      }
      if (!warnedMissing) {
        warnedMissing = true;
        console.warn(
          `rate-limit binding ${bindingName} missing — failing closed`,
        );
      }
      return c.json(
        {
          error: "rate limit unavailable",
          message: `required rate-limit binding ${bindingName} is not configured`,
        },
        503,
      );
    }
    const key = await keyFrom(c);
    const { success } = await bucket.limit({ key });
    if (!success) {
      return c.json(
        {
          error: "rate limit exceeded",
          retry_after_seconds: retryAfter,
        },
        429,
        { "Retry-After": String(retryAfter) },
      );
    }
    return next();
  };
}

/**
 * Extract the caller's IP, preferring `CF-Connecting-IP` (Cloudflare's own
 * header) over `X-Forwarded-For` (which spoofs trivially on non-CF hosts).
 * Falls back to a literal `"unknown"` so a spoofed missing header cannot
 * bypass the limiter entirely — every unknown-IP request shares one bucket.
 */
export function clientIp<E extends AnyEnv = { Bindings: Env }>(
  c: Context<E>,
): string {
  return (
    c.req.header("CF-Connecting-IP") ||
    c.req.header("X-Real-IP") ||
    (c.req.header("X-Forwarded-For") || "").split(",")[0].trim() ||
    "unknown"
  );
}
