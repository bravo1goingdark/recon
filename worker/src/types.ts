/**
 * The slice of Cloudflare's Rate-Limit API we use. The official binding type
 * lives under `@cloudflare/workers-types/experimental` and has drifted across
 * versions; inlining our minimal shape keeps us stable across upgrades.
 */
export interface RateLimitBinding {
  limit(options: { key: string }): Promise<{ success: boolean }>;
}

/** Cloudflare Worker environment bindings. */
export interface Env {
  RECON_DB: D1Database;
  GITHUB_CLIENT_ID: string;
  GITHUB_CLIENT_SECRET: string;
  RAZORPAY_KEY_ID: string;
  RAZORPAY_KEY_SECRET: string;
  RAZORPAY_WEBHOOK_SECRET: string;
  SESSION_SIGNING_KEY: string;
  ALLOWED_ORIGINS: string;
  FRONTEND_URL: string;
  /** HMAC-SHA256 key used to sign license responses. Set via `wrangler secret put LICENSE_HMAC_SECRET`. */
  LICENSE_HMAC_SECRET: string;
  /** Modal-deployed embed endpoint, e.g. `https://<app>.modal.run`. Set via `wrangler secret put MODAL_EMBED_URL`. Optional so local dev / tests can mock fetch. */
  MODAL_EMBED_URL?: string;
  /** Bearer token shared with the Modal `recon-embed-auth` secret. Set via `wrangler secret put MODAL_AUTH_TOKEN`. Optional for the same reason. */
  MODAL_AUTH_TOKEN?: string;
  // Rate-limit bindings — see worker/wrangler.toml for period/limit config.
  // All optional so local dev and tests don't need the bindings provisioned.
  RL_CHECKOUT?: RateLimitBinding;
  RL_WEBHOOK?: RateLimitBinding;
  RL_LICENSE?: RateLimitBinding;
  RL_DASHBOARD?: RateLimitBinding;
  RL_ACCOUNT?: RateLimitBinding;
  RL_EMBED?: RateLimitBinding;
  /** KV namespace for /v1/embed chunk-vector cache. Key = "v1:" + sha256(text); value = JSON-stringified Vec<f32>. TTL 30 days per entry. Optional so tests can run without binding setup. */
  EMBED_CACHE?: KVNamespace;
}

/** D1 row: users table. */
export interface UserRow {
  id: string;
  github_id: number;
  github_username: string;
  email: string | null;
  avatar_url: string | null;
  tier: string;
  created_at: string;
  updated_at: string;
}

/** D1 row: api_keys table. */
export interface ApiKeyRow {
  id: string;
  user_id: string;
  key_hash: string;
  key_prefix: string;
  name: string;
  tier: string;
  limits_json: string;
  expires_at: string | null;
  created_at: string;
  revoked_at: string | null;
}

/** D1 row: sessions table. */
export interface SessionRow {
  id: string;
  user_id: string;
  token_hash: string;
  expires_at: string;
  created_at: string;
}

/** API response matching the Rust CLI's LicenseResponse (license.rs:24-37). */
export interface LicenseValidateResponse {
  valid: boolean;
  tier: string;
  limits: { max_repos: number; max_files: number; max_loc: number };
  expires_at: number;
  message: string;
  /** HMAC-SHA256 over "{tier}:{max_repos}:{max_files}:{max_loc}:{expires_at}". */
  signature: string;
}

/** Authenticated user context set by auth middleware. */
export interface AuthUser {
  id: string;
  github_username: string;
  email: string | null;
  avatar_url: string | null;
  tier: string;
}
