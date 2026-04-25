import { Hono, type Context } from "hono";
import {
  createPlan,
  createSubscription,
  cancelSubscription,
  verifyWebhookSignature,
} from "../lib/razorpay";
import { getTierConfig, getTierPrice, type Currency } from "../lib/tiers";
import { requireAuth } from "../middleware/auth";
import { clientIp, rateLimit } from "../middleware/ratelimit";
import {
  SubscribeBody,
  parseBody,
  RazorpayWebhookBody,
} from "../schemas";
import type { AuthUser, Env } from "../types";

type AuthedEnv = { Bindings: Env; Variables: { user: AuthUser } };

export const billingRoutes = new Hono<AuthedEnv>();

/**
 * POST /v1/billing/subscribe — start a recurring monthly subscription.
 *
 * Creates a Razorpay subscription against the tier's Plan (lazily created
 * the first time we see that tier) and returns Razorpay's hosted-checkout
 * `short_url`. The client redirects the browser there; Razorpay handles
 * card entry + the first charge, then fires `subscription.activated` back
 * to our webhook, which flips the user's tier.
 *
 * Refuses if the user already has an active subscription — upgrade/downgrade
 * between tiers is a separate flow we haven't built yet (v0.1 ships with
 * cancel-and-resubscribe only).
 */
billingRoutes.post(
  "/subscribe",
  requireAuth,
  rateLimit<AuthedEnv>(
    "RL_CHECKOUT",
    (c) => c.get("user")?.id ?? clientIp(c),
    60,
  ),
  async (c) => {
    const user = c.get("user");
    const db = c.env.RECON_DB;

    let rawBody: unknown;
    try {
      rawBody = await c.req.json();
    } catch {
      return c.json({ error: "body must be valid JSON" }, 400);
    }
    const parsed = parseBody(SubscribeBody, rawBody);
    if (!parsed.ok) return c.json(parsed.error, 400);

    // Currency resolution. Explicit body override wins. Otherwise default
    // from Cloudflare's IP-geolocation header: IN → INR (so UPI AutoPay +
    // Net Banking eNACH work natively), everywhere else → USD. Tests and
    // local dev lack the cf.country field, so fall back to USD there too.
    //
    // PPP guard: INR prices are intentionally lower (purchasing-power
    // parity for India). A non-Indian user claiming `currency: "INR"` in
    // the POST body would effectively pay ~75% less than intended. Block
    // that explicitly — the UI already hides the INR toggle for non-IN
    // visitors, so this is the server-side backstop for curl/scripted
    // requests. Missing `cf` (tests, local dev) is treated as non-IN.
    const callerCountry = getCfCountry(c.req.raw);
    if (parsed.value.currency === "INR" && callerCountry !== "IN") {
      return c.json(
        {
          error:
            "INR pricing is only available to subscribers in India. Please subscribe in USD.",
        },
        403,
      );
    }
    const currency: Currency =
      parsed.value.currency ?? resolveDefaultCurrency(c.req.raw);

    const tierConfig = getTierConfig(parsed.value.tier);
    const price = getTierPrice(tierConfig.name, currency);
    if (!price) {
      return c.json(
        {
          error: `Tier ${tierConfig.name} is not priced in ${currency}`,
        },
        400,
      );
    }

    // Race-free placeholder INSERT.
    //
    // The old shape was SELECT-existing → call Razorpay → INSERT row. Two
    // concurrent clicks (or a retried network request) sailed through the
    // SELECT together, double-charged the user upstream, and double-INSERTed.
    //
    // We now claim the slot with a single atomic statement: the INSERT-SELECT
    // body's WHERE NOT EXISTS is evaluated inside the same SQLite write txn
    // as the row insert, so only one of N concurrent callers wins the
    // placeholder. The losers see an empty RETURNING and bail with 409.
    //
    // We also keep the cancel-at-period-end carve-out: rows already flagged
    // for cancellation don't block a new subscribe (cancelled-then-resubscribe
    // is a legitimate flow).
    const placeholder = await db
      .prepare(
        `INSERT INTO subscriptions (user_id, tier, status)
         SELECT ?, ?, 'created'
         WHERE NOT EXISTS (
           SELECT 1 FROM subscriptions
           WHERE user_id = ?
             AND status IN ('created','authenticated','active','pending','halted')
             AND cancel_at_period_end = 0
         )
         RETURNING id`,
      )
      .bind(user.id, tierConfig.name, user.id)
      .first<{ id: string }>();
    if (!placeholder) {
      // Re-read whatever blocked us so the 409 has the same shape clients
      // already depend on (`existing_tier` / `existing_status`).
      const existing = await db
        .prepare(
          `SELECT tier, status FROM subscriptions
           WHERE user_id = ?
             AND status IN ('created','authenticated','active','pending','halted')
             AND cancel_at_period_end = 0
           LIMIT 1`,
        )
        .bind(user.id)
        .first<{ tier: string; status: string }>();
      return c.json(
        {
          error:
            "You already have an active subscription. Cancel it from the dashboard before subscribing to a different tier.",
          existing_tier: existing?.tier ?? null,
          existing_status: existing?.status ?? null,
        },
        409,
      );
    }

    // The catch scope is narrow on purpose. We DELETE the placeholder only
    // when the failure happens *before* Razorpay has materialised a real
    // subscription upstream — i.e. plan creation or createSubscription
    // itself threw. After createSubscription resolves, Razorpay owns a real
    // sub and a real upcoming charge; the placeholder MUST stay so the
    // eventual webhook can self-heal via notes.placeholder_id even if our
    // post-Razorpay UPDATE fails. Wrapping the UPDATE in this try would
    // make a transient D1 hiccup orphan the user's charge.
    let sub: Awaited<ReturnType<typeof createSubscription>>;
    try {
      // Resolve (or lazily create) the Razorpay plan for this (tier, currency).
      const planId = await ensurePlanForTier(
        db,
        c.env.RAZORPAY_KEY_ID,
        c.env.RAZORPAY_KEY_SECRET,
        tierConfig.name,
        currency,
        price.amount,
      );

      // 120 cycles = 10 years of monthly charges. Effectively unbounded but
      // Razorpay requires a finite total_count.
      //
      // We pass `placeholder_id` in the notes so the webhook can self-heal:
      // if our post-Razorpay UPDATE below fails (network drop, isolate kill),
      // the eventual subscription.activated webhook still finds the row by
      // joining on notes.placeholder_id.
      //
      // callback_url is what Razorpay redirects to after the user finishes
      // authorising the mandate. Without it they get stranded on Razorpay's
      // hosted "Payment successful" page. `?just_paid=1` flags the dashboard
      // to poll /v1/billing/portal for a few seconds until the webhook lands.
      const callbackUrl = `${c.env.FRONTEND_URL.replace(/\/$/, "")}/dashboard?just_paid=1`;
      sub = await createSubscription(
        c.env.RAZORPAY_KEY_ID,
        c.env.RAZORPAY_KEY_SECRET,
        {
          plan_id: planId,
          total_count: 120,
          customer_notify: true,
          notes: {
            user_id: user.id,
            tier: tierConfig.name,
            currency,
            placeholder_id: placeholder.id,
          },
          callback_url: callbackUrl,
          callback_method: "get",
        },
      );
    } catch (err) {
      // Pre-Razorpay failure (plan creation or createSubscription threw): no
      // upstream sub exists, so delete the placeholder to unblock retry.
      // Only delete if it's still ours — a webhook that already self-healed
      // the row (raced ahead of this catch in some bizarre ordering) must
      // not be clobbered.
      await db
        .prepare(
          `DELETE FROM subscriptions
           WHERE id = ? AND razorpay_subscription_id IS NULL`,
        )
        .bind(placeholder.id)
        .run();
      throw err;
    }

    // Idempotent UPDATE: only stamps the razorpay_subscription_id when the
    // row is still ours (NULL razorpay_subscription_id) or was previously
    // stamped with the same id (webhook self-heal racing this UPDATE).
    // Never overwrites a different sub_id.
    //
    // If this throws, we deliberately DO NOT delete the placeholder — the
    // Razorpay sub exists upstream and the webhook will self-heal the row
    // via notes.placeholder_id. Letting the error bubble surfaces a 500 to
    // the caller, but the user's billing state recovers automatically.
    await db
      .prepare(
        `UPDATE subscriptions
         SET razorpay_subscription_id = ?
         WHERE id = ?
           AND (razorpay_subscription_id IS NULL OR razorpay_subscription_id = ?)`,
      )
      .bind(sub.id, placeholder.id, sub.id)
      .run();

    return c.json({
      subscription_id: sub.id,
      short_url: sub.short_url,
      tier: tierConfig.name,
      currency,
      price_display: price.display,
    });
  },
);

/**
 * POST /v1/billing/cancel — schedule cancel-at-cycle-end on the user's
 * active subscription.
 *
 * Honor-until-period-end: access stays on the current tier until
 * `current_period_end`, no further charges happen, and the hourly cron
 * downgrades the user's api_keys to Free once `expires_at` passes.
 */
billingRoutes.post(
  "/cancel",
  requireAuth,
  rateLimit<AuthedEnv>(
    "RL_CHECKOUT",
    (c) => c.get("user")?.id ?? clientIp(c),
    60,
  ),
  async (c) => {
    const user = c.get("user");
    const db = c.env.RECON_DB;

    const sub = await db
      .prepare(
        `SELECT id, razorpay_subscription_id, tier, status,
                current_period_end, cancel_at_period_end
         FROM subscriptions
         WHERE user_id = ? AND status IN ('authenticated','active','pending','halted')
         ORDER BY created_at DESC LIMIT 1`,
      )
      .bind(user.id)
      .first<{
        id: string;
        razorpay_subscription_id: string | null;
        tier: string;
        status: string;
        current_period_end: string | null;
        cancel_at_period_end: number;
      }>();

    if (!sub) {
      return c.json(
        { error: "No active subscription to cancel" },
        404,
      );
    }

    if (sub.cancel_at_period_end) {
      return c.json({
        status: "already_scheduled",
        access_until: sub.current_period_end,
      });
    }

    if (!sub.razorpay_subscription_id) {
      return c.json(
        { error: "Subscription not yet registered with Razorpay" },
        409,
      );
    }

    // Ask Razorpay to stop renewals at the end of the current cycle.
    // This does NOT refund or end access immediately.
    await cancelSubscription(
      c.env.RAZORPAY_KEY_ID,
      c.env.RAZORPAY_KEY_SECRET,
      sub.razorpay_subscription_id,
      true,
    );

    await db
      .prepare(
        `UPDATE subscriptions
         SET cancel_at_period_end = 1,
             cancelled_at = datetime('now')
         WHERE id = ?`,
      )
      .bind(sub.id)
      .run();

    return c.json({
      status: "scheduled",
      access_until: sub.current_period_end,
      tier: sub.tier,
    });
  },
);

/**
 * POST /v1/billing/webhook — Razorpay event webhook.
 *
 * Handles:
 *   - subscription.activated     (first charge succeeded — grant tier)
 *   - subscription.charged       (renewal charge — extend expires_at)
 *   - subscription.cancelled     (cycle-end reached after cancel request)
 *   - subscription.halted        (repeated charge failures — preserve access until paid period ends)
 *   - subscription.completed     (total_count reached — same semantics as cancelled)
 *
 * `payment.captured` events also fire upstream (Razorpay sends one per
 * subscription charge), but they don't carry the subscription_id we need
 * to bind the payment to a user's tier. We rely solely on the
 * subscription.* events. The handler ignores payment.captured by
 * dropping it through to the default branch.
 *
 * Subscription events don't carry a unique payment_id; we rely on the
 * subscription row's status monotonically progressing — replays set the
 * same state and are harmless.
 */
billingRoutes.post(
  "/webhook",
  rateLimit("RL_WEBHOOK", (c) => clientIp(c), 60),
  async (c) => {
    const body = await c.req.text();
    const signature = c.req.header("X-Razorpay-Signature");
    if (!signature) return c.json({ error: "Missing signature" }, 400);

    const valid = await verifyWebhookSignature(
      body,
      signature,
      c.env.RAZORPAY_WEBHOOK_SECRET,
    );
    if (!valid) return c.json({ error: "Invalid signature" }, 400);

    let eventRaw: unknown;
    try {
      eventRaw = JSON.parse(body);
    } catch {
      return c.json({ error: "webhook body is not JSON" }, 400);
    }
    const parsed = parseBody(RazorpayWebhookBody, eventRaw);
    if (!parsed.ok) return c.json(parsed.error, 400);
    const event = parsed.value;
    const db = c.env.RECON_DB;

    // Replay-window guard. HMAC alone says "this body was signed with our
    // secret at some point" — it doesn't bound *when*. A captured-and-
    // replayed payload from weeks ago verifies the same as a fresh one.
    // Razorpay sets `created_at` on every event; we accept events at most
    // 24 hours old (Razorpay retries delivery for ~24h after a failure,
    // each retry carrying the *original* created_at). Tightening below
    // 24h would reject legitimate retries on prolonged outages.
    //
    // 5-minute future skew tolerance accommodates clock drift between
    // Razorpay's edge and Cloudflare's runtime.
    const WEBHOOK_REPLAY_MAX_AGE_SEC = 24 * 60 * 60;
    const WEBHOOK_REPLAY_FUTURE_SKEW_SEC = 5 * 60;
    if (typeof event.created_at !== "number") {
      // Audit-only: this is a malformed body, not a replay attack. Skip
      // the dropped-events row to keep that table focused on the cases
      // worth investigating.
      return c.json({ error: "Webhook event missing created_at" }, 400);
    }
    const nowSec = Math.floor(Date.now() / 1000);
    const ageSec = nowSec - event.created_at;
    if (ageSec > WEBHOOK_REPLAY_MAX_AGE_SEC || -ageSec > WEBHOOK_REPLAY_FUTURE_SKEW_SEC) {
      // Record the dropped delivery so the audit trail surfaces stale
      // replays — the alternative is losing the event silently. We do
      // this BEFORE the early-return so the row lands even on the 400.
      const subId =
        event.payload.subscription?.entity?.id ??
        event.payload.payment?.entity?.id ??
        null;
      await recordDroppedEvent(db, event, "replay_window_exceeded", subId);
      return c.json(
        {
          error: "Webhook event outside replay window",
          age_seconds: ageSec,
        },
        400,
      );
    }

    // Event-ID idempotency. Razorpay sets a unique `X-Razorpay-Event-Id`
    // header on every delivery. INSERT-OR-IGNORE on the PK lets us tell
    // whether we've handled this exact delivery before; a duplicate
    // returns 2xx with a deterministic body so Razorpay stops retrying.
    //
    // We dedup BEFORE side effects so retried events never re-run the
    // tier/api_keys cascade. (The status-guard fix in handleSubscriptionCharged
    // is defense-in-depth; this is the first line.)
    const eventId = c.req.header("X-Razorpay-Event-Id");
    if (eventId) {
      const inserted = await db
        .prepare(
          `INSERT INTO webhook_events_seen (event_id, event_type)
           VALUES (?, ?)
           ON CONFLICT(event_id) DO NOTHING
           RETURNING event_id`,
        )
        .bind(eventId, event.event)
        .first<{ event_id: string }>();
      if (!inserted) {
        return c.json({
          status: "already_processed",
          event_id: eventId,
        });
      }
    }
    // Missing header (legacy/manual replay tooling): we still process the
    // event since the rest of the guard pipeline is sufficient. This isn't
    // a documented Razorpay behaviour — they always send the header — but
    // fail-open keeps debugging the webhook by hand workable.

    switch (event.event) {
      case "subscription.activated":
      case "subscription.charged":
        return handleSubscriptionCharged(c, event, db);
      case "subscription.cancelled":
      case "subscription.completed":
        return handleSubscriptionCancelled(c, event, db);
      case "subscription.halted":
        return handleSubscriptionHalted(c, event, db);
      default:
        return c.json({ status: "ignored", event: event.event });
    }
  },
);

/** GET /v1/billing/portal — subscription status + next billing info. */
billingRoutes.get("/portal", requireAuth, async (c) => {
  const user = c.get("user");
  const db = c.env.RECON_DB;

  const sub = await db
    .prepare(
      `SELECT tier, status, current_period_start, current_period_end,
              cancel_at_period_end, cancelled_at
       FROM subscriptions
       WHERE user_id = ?
       ORDER BY created_at DESC LIMIT 1`,
    )
    .bind(user.id)
    .first<{
      tier: string;
      status: string;
      current_period_start: string | null;
      current_period_end: string | null;
      cancel_at_period_end: number;
      cancelled_at: string | null;
    }>();

  return c.json({
    tier: user.tier,
    tier_config: getTierConfig(user.tier),
    subscription: sub
      ? {
          tier: sub.tier,
          status: sub.status,
          current_period_start: sub.current_period_start,
          current_period_end: sub.current_period_end,
          cancel_at_period_end: !!sub.cancel_at_period_end,
          cancelled_at: sub.cancelled_at,
        }
      : null,
  });
});

// ────────────────────────────────────────────────────────────────────────────
// Internal webhook handlers — one per event shape.
// Keeping these separate from the dispatch switch makes each handler's
// happy-path short enough to read end-to-end.
// ────────────────────────────────────────────────────────────────────────────

type WebhookCtx = Context<AuthedEnv>;

async function handleSubscriptionCharged(
  c: WebhookCtx,
  event: RazorpayWebhookBody,
  db: D1Database,
) {
  const sub = event.payload.subscription?.entity;
  if (!sub) {
    return c.json({ status: "ignored", reason: "no subscription entity" });
  }

  // Reject events we cannot safely act on. Each branch records the dropped
  // event for audit (Workers `console.warn` vanishes without log forwarding).
  // Webhook still 2xx so Razorpay does not retry-storm a known-bad event.
  if (sub.current_end == null) {
    // Razorpay occasionally fires subscription events before the first
    // billing cycle is finalized — current_end is null in those. Granting
    // tier with NULL expires_at would write NULL into api_keys.expires_at,
    // and the hourly downgrade cron skips NULL by design (it's used for
    // never-expiring CI keys). Net effect of the bug: permanent free Pro.
    await recordDroppedEvent(db, event, "missing_current_end", sub.id);
    return c.json({
      status: "dropped",
      reason: "missing_current_end",
      subscription_id: sub.id,
    });
  }

  // Locate our row. First by razorpay_subscription_id (the steady state),
  // then by notes.placeholder_id (webhook self-heal: the post-Razorpay
  // UPDATE in /subscribe failed and never stamped sub_id onto the row).
  let row = await db
    .prepare(
      `SELECT id, user_id, tier FROM subscriptions
       WHERE razorpay_subscription_id = ?`,
    )
    .bind(sub.id)
    .first<{ id: string; user_id: string; tier: string }>();

  if (!row && sub.notes?.placeholder_id) {
    const placeholder = await db
      .prepare(
        `SELECT id, user_id, tier FROM subscriptions
         WHERE id = ? AND razorpay_subscription_id IS NULL`,
      )
      .bind(sub.notes.placeholder_id)
      .first<{ id: string; user_id: string; tier: string }>();
    if (placeholder) {
      // Bind the sub_id idempotently so subsequent webhook deliveries find
      // the row via the primary path.
      await db
        .prepare(
          `UPDATE subscriptions
           SET razorpay_subscription_id = ?
           WHERE id = ? AND razorpay_subscription_id IS NULL`,
        )
        .bind(sub.id, placeholder.id)
        .run();
      row = placeholder;
    }
  }

  if (!row) {
    await recordDroppedEvent(db, event, "unknown_subscription", sub.id);
    return c.json({ status: "unknown_subscription", subscription_id: sub.id });
  }

  const periodStart = sub.current_start
    ? isoFromUnix(sub.current_start)
    : null;
  const periodEnd = isoFromUnix(sub.current_end);
  const limits = JSON.stringify(getTierConfig(row.tier).limits);

  // Status guard. Razorpay delivers webhooks at-least-once and may reorder
  // them on retry — a delayed `charged` arriving after a `cancelled` would
  // (under the old code) flip the row back to active and extend service for
  // a billing period the user never paid for.
  //
  // The UPDATE only fires for non-terminal rows. We then read meta.changes
  // to decide whether the cascading user/api_keys updates should run.
  const updateResult = await db
    .prepare(
      `UPDATE subscriptions
       SET status = 'active',
           current_period_start = COALESCE(?, current_period_start),
           current_period_end = COALESCE(?, current_period_end)
       WHERE id = ?
         AND status NOT IN ('cancelled','completed','expired')`,
    )
    .bind(periodStart, periodEnd, row.id)
    .run();

  if (updateResult.meta.changes === 0) {
    await recordDroppedEvent(db, event, "subscription_terminal", sub.id);
    return c.json({
      status: "terminal_skipped",
      subscription_id: sub.id,
    });
  }

  await db.batch([
    db
      .prepare(
        "UPDATE users SET tier = ?, updated_at = datetime('now') WHERE id = ?",
      )
      .bind(row.tier, row.user_id),
    db
      .prepare(
        `UPDATE api_keys SET tier = ?, limits_json = ?, expires_at = ?
         WHERE user_id = ? AND revoked_at IS NULL`,
      )
      .bind(row.tier, limits, periodEnd, row.user_id),
  ]);

  return c.json({
    status: "ok",
    subscription_id: sub.id,
    period_end: periodEnd,
  });
}

/**
 * Record a webhook event we deliberately dropped. Workers `console.warn`
 * vanishes without log forwarding; the row IS the audit trail. Failures
 * here are swallowed — never let an audit-write block the webhook 2xx.
 */
async function recordDroppedEvent(
  db: D1Database,
  event: RazorpayWebhookBody,
  reason: string,
  subscriptionId: string | null,
): Promise<void> {
  try {
    await db
      .prepare(
        `INSERT INTO webhook_events_dropped
           (event_type, razorpay_subscription_id, reason, payload_json)
         VALUES (?, ?, ?, ?)`,
      )
      .bind(event.event, subscriptionId, reason, JSON.stringify(event))
      .run();
  } catch {
    // Audit insert failed — fall through. The webhook handler still
    // returns 2xx; we just lose this one breadcrumb.
  }
}

async function handleSubscriptionCancelled(
  c: WebhookCtx,
  event: RazorpayWebhookBody,
  db: D1Database,
) {
  const sub = event.payload.subscription?.entity;
  if (!sub) {
    return c.json({ status: "ignored", reason: "no subscription entity" });
  }

  // Do NOT expire api_keys here — the user paid for the current cycle and
  // keeps access until current_period_end. The hourly cron will downgrade
  // once that time passes. We only flip the subscription row's status so
  // /portal reflects reality and future charges don't happen.
  await db
    .prepare(
      `UPDATE subscriptions
       SET status = 'cancelled',
           cancelled_at = COALESCE(cancelled_at, datetime('now'))
       WHERE razorpay_subscription_id = ?`,
    )
    .bind(sub.id)
    .run();

  return c.json({ status: "ok", subscription_id: sub.id });
}

async function handleSubscriptionHalted(
  c: WebhookCtx,
  event: RazorpayWebhookBody,
  db: D1Database,
) {
  const sub = event.payload.subscription?.entity;
  if (!sub) {
    return c.json({ status: "ignored", reason: "no subscription entity" });
  }

  // Razorpay has stopped trying to charge after repeated failures. Access
  // continues until the already-paid current_period_end, same as cancelled
  // — the user just can't re-activate without a fresh card.
  await db
    .prepare(
      `UPDATE subscriptions
       SET status = 'halted'
       WHERE razorpay_subscription_id = ?`,
    )
    .bind(sub.id)
    .run();

  return c.json({ status: "ok", subscription_id: sub.id });
}

/**
 * Resolve a Razorpay plan_id for a (tier, currency) pair, creating it in
 * Razorpay (and caching the ID in D1) on first use. Subsequent subscribers
 * to the same (tier, currency) reuse the cached ID; a different currency
 * triggers a fresh plan creation because Razorpay plans are
 * currency-specific.
 *
 * There is a benign race: two simultaneous first-subscribers for the same
 * (tier, currency) could each call createPlan, yielding two orphan plans
 * upstream. The ON CONFLICT(tier, currency) DO NOTHING + re-read pattern
 * settles the race so everyone sees the same plan_id on their next request.
 */
async function ensurePlanForTier(
  db: D1Database,
  keyId: string,
  keySecret: string,
  tierName: string,
  currency: Currency,
  amount: number,
): Promise<string> {
  const cached = await db
    .prepare(
      "SELECT razorpay_plan_id FROM subscription_plans WHERE tier = ? AND currency = ?",
    )
    .bind(tierName, currency)
    .first<{ razorpay_plan_id: string }>();
  if (cached?.razorpay_plan_id) return cached.razorpay_plan_id;

  const plan = await createPlan(keyId, keySecret, {
    tier: tierName,
    amount,
    currency,
    period: "monthly",
    interval: 1,
    description: planDescriptionFor(tierName),
  });

  await db
    .prepare(
      `INSERT INTO subscription_plans
         (tier, currency, razorpay_plan_id, amount, interval_period, interval_count)
       VALUES (?, ?, ?, ?, 'monthly', 1)
       ON CONFLICT(tier, currency) DO NOTHING`,
    )
    .bind(tierName, currency, plan.id, amount)
    .run();

  // Re-read to resolve the race described above — whoever actually inserted
  // wins, so everyone ends up seeing the same plan_id.
  const winner = await db
    .prepare(
      "SELECT razorpay_plan_id FROM subscription_plans WHERE tier = ? AND currency = ?",
    )
    .bind(tierName, currency)
    .first<{ razorpay_plan_id: string }>();
  return winner?.razorpay_plan_id ?? plan.id;
}

/**
 * Choose the default billing currency for an incoming subscribe request
 * when the body hasn't specified one. Cloudflare's `cf.country` field is
 * populated in production; absent in tests and local dev, in which case
 * we default to USD (matches the pricing page's non-IN default).
 */
function resolveDefaultCurrency(req: Request): Currency {
  return getCfCountry(req) === "IN" ? "INR" : "USD";
}

/**
 * Read Cloudflare's IP-geolocated country code off the request. Returns
 * undefined when the `cf` object is absent (tests, local dev, non-CF
 * proxies) — callers must treat undefined as "not India" for PPP gating
 * so a missing header can't be used to claim INR eligibility.
 */
function getCfCountry(req: Request): string | undefined {
  // Cloudflare attaches a non-standard `cf` object to incoming Requests at
  // runtime. It's typed minimally by workers-types as `CfProperties`; we
  // reach for `country` specifically.
  return (req as unknown as { cf?: { country?: string } }).cf?.country;
}

function isoFromUnix(unixSeconds: number): string {
  return new Date(unixSeconds * 1000).toISOString();
}

/**
 * Customer-facing plan description shown on Razorpay's hosted checkout
 * and the receipt email. Locked at plan-creation time, so any edit here
 * only takes effect for *new* (tier, currency) plans — purge
 * subscription_plans before the next subscriber wants the new copy.
 *
 * Numbers mirror tiers.ts. Kept compact (single line) because Razorpay's
 * checkout page truncates long descriptions.
 */
function planDescriptionFor(tierName: string): string {
  switch (tierName) {
    case "Pro":
      return "Up to 10 repos · 5,000 files/repo · 200K LOC. Priority support. Cancel anytime, access continues until period end.";
    case "Team":
      return "Up to 25 repos · 50,000 files/repo · 4M LOC. Priority support. Cancel anytime, access continues until period end.";
    default:
      return `recon ${tierName} — monthly subscription`;
  }
}
