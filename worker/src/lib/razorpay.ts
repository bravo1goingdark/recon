import { hmacSha256Hex, timingSafeEqual } from "./crypto";

const RAZORPAY_BASE = "https://api.razorpay.com/v1";

// ────────────────────────────────────────────────────────────────────────────
// One-time Orders (legacy — kept for backwards compat; callers should move to
// Subscriptions for anything recurring).
// ────────────────────────────────────────────────────────────────────────────

export interface RazorpayOrder {
  id: string;
  amount: number;
  currency: string;
  receipt: string;
  status: string;
}

/** Create a Razorpay order. */
export async function createOrder(
  keyId: string,
  keySecret: string,
  amount: number,
  currency: string,
  receipt: string,
  notes: Record<string, string>,
): Promise<RazorpayOrder> {
  return rpPost<RazorpayOrder>(keyId, keySecret, "/orders", {
    amount,
    currency,
    receipt,
    notes,
  });
}

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

async function rpPost<T>(
  keyId: string,
  keySecret: string,
  path: string,
  body: unknown,
): Promise<T> {
  const resp = await fetch(`${RAZORPAY_BASE}${path}`, {
    method: "POST",
    headers: {
      Authorization: "Basic " + btoa(`${keyId}:${keySecret}`),
      "Content-Type": "application/json",
    },
    body: JSON.stringify(body),
  });
  if (!resp.ok) {
    const err = await resp.text();
    throw new Error(`Razorpay ${path} failed: ${resp.status} ${err}`);
  }
  return (await resp.json()) as T;
}

async function rpGet<T>(
  keyId: string,
  keySecret: string,
  path: string,
): Promise<T> {
  const resp = await fetch(`${RAZORPAY_BASE}${path}`, {
    headers: {
      Authorization: "Basic " + btoa(`${keyId}:${keySecret}`),
    },
  });
  if (!resp.ok) {
    const err = await resp.text();
    throw new Error(`Razorpay GET ${path} failed: ${resp.status} ${err}`);
  }
  return (await resp.json()) as T;
}
