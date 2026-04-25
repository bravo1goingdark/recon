import { hmacSha256Hex, timingSafeEqual } from "./crypto";

const RAZORPAY_BASE = "https://api.razorpay.com/v1";

// ────────────────────────────────────────────────────────────────────────────
// Plans — tier-level pricing objects that subscriptions reference.
// One Plan per tier; created lazily on first subscribe.
// ────────────────────────────────────────────────────────────────────────────

export interface RazorpayPlan {
  id: string;
  entity: "plan";
  period: string;
  interval: number;
  item: {
    id: string;
    name: string;
    amount: number;
    currency: string;
  };
}

/**
 * Create a Razorpay plan.
 *
 * `period` is one of "daily" | "weekly" | "monthly" | "yearly"; `interval`
 * is how many of those to wait between charges. For our purposes: monthly, 1.
 */
export async function createPlan(
  keyId: string,
  keySecret: string,
  opts: {
    tier: string;
    amount: number;
    currency: string;
    period: "daily" | "weekly" | "monthly" | "yearly";
    interval: number;
  },
): Promise<RazorpayPlan> {
  return rpPost<RazorpayPlan>(keyId, keySecret, "/plans", {
    period: opts.period,
    interval: opts.interval,
    item: {
      name: `recon ${opts.tier}`,
      description: `recon ${opts.tier} — ${opts.period} subscription`,
      amount: opts.amount,
      currency: opts.currency,
    },
    notes: { tier: opts.tier },
  });
}

// ────────────────────────────────────────────────────────────────────────────
// Subscriptions — recurring charges tied to a Plan.
// ────────────────────────────────────────────────────────────────────────────

export interface RazorpaySubscription {
  id: string;
  entity: "subscription";
  plan_id: string;
  status: string; // created | authenticated | active | pending | halted | cancelled | completed | expired
  current_start: number | null;
  current_end: number | null;
  ended_at: number | null;
  quantity: number;
  notes: Record<string, string>;
  charge_at: number | null;
  start_at: number | null;
  end_at: number | null;
  auth_attempts: number;
  total_count: number;
  paid_count: number;
  customer_notify: boolean;
  created_at: number;
  short_url: string;
  has_scheduled_changes: boolean;
  change_scheduled_at: number | null;
}

/**
 * Create a subscription. Razorpay returns a `short_url` — the hosted checkout
 * URL we redirect the user to. After they complete the first charge, the
 * `subscription.activated` webhook fires.
 *
 * `total_count` is the cap on number of billing cycles. 120 = 10 years of
 * monthly charges; effectively unbounded for our purposes.
 */
export async function createSubscription(
  keyId: string,
  keySecret: string,
  opts: {
    plan_id: string;
    total_count: number;
    customer_notify: boolean;
    notes: Record<string, string>;
  },
): Promise<RazorpaySubscription> {
  return rpPost<RazorpaySubscription>(keyId, keySecret, "/subscriptions", {
    plan_id: opts.plan_id,
    total_count: opts.total_count,
    customer_notify: opts.customer_notify ? 1 : 0,
    notes: opts.notes,
  });
}

/**
 * Cancel a subscription.
 *
 * `cancelAtCycleEnd = true` is the honor-until-period-end path: the user
 * keeps access until `current_end`, no further charges happen, and the
 * `subscription.cancelled` webhook fires after the current cycle completes.
 * `cancelAtCycleEnd = false` terminates immediately and issues a refund if
 * applicable — not what we want for self-service cancel.
 */
export async function cancelSubscription(
  keyId: string,
  keySecret: string,
  subscriptionId: string,
  cancelAtCycleEnd: boolean,
): Promise<RazorpaySubscription> {
  return rpPost<RazorpaySubscription>(
    keyId,
    keySecret,
    `/subscriptions/${subscriptionId}/cancel`,
    { cancel_at_cycle_end: cancelAtCycleEnd ? 1 : 0 },
  );
}

/** Fetch the live state of a subscription from Razorpay. */
export async function fetchSubscription(
  keyId: string,
  keySecret: string,
  subscriptionId: string,
): Promise<RazorpaySubscription> {
  return rpGet<RazorpaySubscription>(
    keyId,
    keySecret,
    `/subscriptions/${subscriptionId}`,
  );
}

// ────────────────────────────────────────────────────────────────────────────
// Webhook signature verification.
// ────────────────────────────────────────────────────────────────────────────

/** Verify Razorpay webhook signature (HMAC-SHA256). */
export async function verifyWebhookSignature(
  body: string,
  signature: string,
  secret: string,
): Promise<boolean> {
  const expected = await hmacSha256Hex(secret, body);
  return timingSafeEqual(expected, signature);
}

// ────────────────────────────────────────────────────────────────────────────
// Internal HTTP helpers. Centralize auth + error shape so callers don't
// duplicate the basic-auth header and the "throw on !ok" dance.
// ────────────────────────────────────────────────────────────────────────────

/**
 * Retry policy. Razorpay's HTTP edge occasionally 502s during deploys
 * and on transient network blips — retrying once or twice with backoff
 * is the difference between a noisy 5xx to the user and a successful
 * subscription. We never retry 4xx (auth, bad request — won't change),
 * never retry idempotent ops more than RETRY_COUNT times (creates a real
 * sub upstream each time createSubscription succeeds — must NOT replay
 * a partial success), and bound each attempt with a hard timeout so a
 * stuck connection can't pin a Worker isolate.
 */
const RETRY_COUNT = 2;
const RETRY_BASE_MS = 250;
const REQUEST_TIMEOUT_MS = 10_000;

interface RpResponse {
  ok: boolean;
  status: number;
  body: string;
}

/**
 * Razorpay HTTP error with a status code attached so retry/branching
 * logic can reason about it without parsing the message string.
 */
export class RazorpayHttpError extends Error {
  status: number;
  responseBody: string;
  constructor(status: number, path: string, body: string) {
    super(`Razorpay ${path} failed: ${status} ${body}`);
    this.name = "RazorpayHttpError";
    this.status = status;
    this.responseBody = body;
  }
}

/**
 * Fetch with an AbortController-bounded timeout. Caller must dispatch
 * the result body — we return raw text + status so retry logic can
 * branch on the HTTP code without re-parsing JSON.
 */
async function fetchWithTimeout(
  url: string,
  init: RequestInit,
): Promise<RpResponse> {
  const ctrl = new AbortController();
  const t = setTimeout(() => ctrl.abort(), REQUEST_TIMEOUT_MS);
  try {
    const resp = await fetch(url, { ...init, signal: ctrl.signal });
    const body = await resp.text();
    return { ok: resp.ok, status: resp.status, body };
  } finally {
    clearTimeout(t);
  }
}

/**
 * Send a Razorpay request with the project's standard retry/backoff and
 * timeout. Retries on:
 *   - network errors (fetch threw — DNS, ECONNRESET, abort/timeout)
 *   - HTTP 5xx (transient upstream failure)
 * Never retries on 4xx — those won't get better, and double-billing risk
 * is real for non-idempotent verbs.
 *
 * The `path` is logged in the error message so callers don't have to
 * pass anything extra; secrets are never echoed.
 */
async function rpRequest<T>(
  method: "GET" | "POST",
  keyId: string,
  keySecret: string,
  path: string,
  body: unknown | undefined,
): Promise<T> {
  const url = `${RAZORPAY_BASE}${path}`;
  const init: RequestInit = {
    method,
    headers: {
      Authorization: "Basic " + btoa(`${keyId}:${keySecret}`),
      ...(body !== undefined ? { "Content-Type": "application/json" } : {}),
    },
    ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
  };

  let lastErr: unknown;
  for (let attempt = 0; attempt <= RETRY_COUNT; attempt++) {
    try {
      const resp = await fetchWithTimeout(url, init);
      if (resp.ok) return JSON.parse(resp.body) as T;
      // 5xx → retry (if attempts remain). 4xx → throw immediately
      // (auth, validation, business rejection — won't change on retry).
      if (resp.status >= 500 && attempt < RETRY_COUNT) {
        lastErr = new RazorpayHttpError(resp.status, `${method} ${path}`, resp.body);
        await sleep(RETRY_BASE_MS * Math.pow(2, attempt));
        continue;
      }
      throw new RazorpayHttpError(resp.status, `${method} ${path}`, resp.body);
    } catch (err) {
      // 4xx must NOT be retried — propagate immediately so the caller
      // (e.g. /subscribe) sees the upstream error and surfaces it.
      if (err instanceof RazorpayHttpError && err.status < 500) throw err;
      lastErr = err;
      if (attempt < RETRY_COUNT) {
        await sleep(RETRY_BASE_MS * Math.pow(2, attempt));
        continue;
      }
    }
  }
  throw lastErr instanceof Error
    ? lastErr
    : new Error(`Razorpay ${method} ${path} failed after retries`);
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

async function rpPost<T>(
  keyId: string,
  keySecret: string,
  path: string,
  body: unknown,
): Promise<T> {
  return rpRequest<T>("POST", keyId, keySecret, path, body);
}

async function rpGet<T>(
  keyId: string,
  keySecret: string,
  path: string,
): Promise<T> {
  return rpRequest<T>("GET", keyId, keySecret, path, undefined);
}
