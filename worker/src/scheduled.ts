/**
 * Scheduled-handler entrypoint for Cloudflare Cron Triggers.
 *
 * Cadence: hourly (see wrangler.toml [triggers]). The job finds api_keys
 * whose `expires_at` has passed and the subscription is no longer paid for,
 * then downgrades them to Free.
 *
 * Honor-until-period-end semantics live *in this file* — by the time a row
 * matches the query, `current_period_end` is already in the past. Cancellation
 * itself is scheduled via Razorpay's cancel_at_cycle_end, which only causes
 * `expires_at` to stop being extended. So this cron is the terminal state of
 * the cancel flow: "user paid through this date, the date has now passed,
 * switch them to Free".
 */

import { getTierConfig } from "./lib/tiers";
import type { Env } from "./types";

const FREE_LIMITS_JSON = JSON.stringify(getTierConfig("Free").limits);

export async function downgradeExpired(db: D1Database): Promise<{
  downgraded_keys: number;
  downgraded_users: number;
}> {
  // Careful comparison: expires_at is written by the webhook handler using
  // `new Date().toISOString()` ("2026-04-24T00:15:00.000Z" — with `T` and
  // trailing `Z`), but SQLite's `datetime('now')` returns
  // "2026-04-24 00:15:00" (space separator, no `Z`). Lexicographic compare
  // would lie ("T" > " "). Coerce both to unix seconds via strftime('%s',…)
  // so we compare apples to apples regardless of which ISO variant lands
  // in the column.
  //
  // `tier != 'Free'` scopes to rows that are actually paid-or-were-paid, so
  // the cron is idempotent — once downgraded to Free, a row no longer matches.
  const { results: expiredKeys = [] } = await db
    .prepare(
      `SELECT id, user_id, tier
       FROM api_keys
       WHERE revoked_at IS NULL
         AND expires_at IS NOT NULL
         AND CAST(strftime('%s', expires_at) AS INTEGER)
             < CAST(strftime('%s', 'now') AS INTEGER)
         AND tier != 'Free'
       LIMIT 500`, // bounded batch; cron re-runs in an hour if there are more
    )
    .all<{ id: string; user_id: string; tier: string }>();

  if (expiredKeys.length === 0) {
    return { downgraded_keys: 0, downgraded_users: 0 };
  }

  const keyIds = expiredKeys.map((r) => r.id);
  const userIds = Array.from(new Set(expiredKeys.map((r) => r.user_id)));

  const keyPlaceholders = keyIds.map(() => "?").join(",");
  const userPlaceholders = userIds.map(() => "?").join(",");

  // Three writes in one atomic batch:
  //   1. Key: tier → Free, limits → Free's, clear expires_at so it's not
  //      re-scanned next hour.
  //   2. Users whose every non-revoked key is now Free get their user-level
  //      tier flipped to Free too. (If a user has *another* still-paid key
  //      we skip them — that shouldn't happen today since we issue one key
  //      per user, but keep the semantics correct for the day we ship
  //      multi-key support.)
  //   3. Subscription row: terminal state, kept as 'cancelled' or 'completed'
  //      by the webhook. We don't touch it here.
  await db.batch([
    db
      .prepare(
        `UPDATE api_keys
         SET tier = 'Free',
             limits_json = ?,
             expires_at = NULL
         WHERE id IN (${keyPlaceholders})`,
      )
      .bind(FREE_LIMITS_JSON, ...keyIds),
    db
      .prepare(
        `UPDATE users
         SET tier = 'Free', updated_at = datetime('now')
         WHERE id IN (${userPlaceholders})
           AND NOT EXISTS (
             SELECT 1 FROM api_keys
             WHERE api_keys.user_id = users.id
               AND api_keys.revoked_at IS NULL
               AND api_keys.tier != 'Free'
           )`,
      )
      .bind(...userIds),
  ]);

  console.log(
    `cron: downgraded ${expiredKeys.length} expired key(s) across ${userIds.length} user(s) to Free`,
  );

  return {
    downgraded_keys: expiredKeys.length,
    downgraded_users: userIds.length,
  };
}

export async function handleScheduled(env: Env): Promise<void> {
  try {
    await downgradeExpired(env.RECON_DB);
  } catch (err) {
    // Never throw from a scheduled handler — just log. Cloudflare retries
    // on thrown errors and the retry cost isn't worth it for an hourly job;
    // the next scheduled tick will catch any rows we missed.
    console.error("cron downgradeExpired failed:", err);
  }
}
