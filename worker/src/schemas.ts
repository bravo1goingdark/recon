/**
 * Zod schemas for every externally-facing POST body.
 *
 * Every route that accepts user or Razorpay input validates through one of
 * these schemas before touching D1. A failed validation returns a structured
 * 400 — we never let malformed bodies reach the business logic.
 */

import { z } from "zod";

/** POST /v1/billing/subscribe — start a recurring subscription for a tier. */
export const SubscribeBody = z.object({
  tier: z.enum(["Pro", "Team"]),
  /**
   * Billing currency. Optional — when absent, the server defaults from the
   * request's Cloudflare-IPCountry header: IN → INR, everything else → USD.
   * Indian users default to INR so UPI AutoPay / Net Banking eNACH work;
   * those rails don't support USD-denominated subscriptions.
   */
  currency: z.enum(["INR", "USD"]).optional(),
});
export type SubscribeBody = z.infer<typeof SubscribeBody>;

/**
 * POST /v1/billing/webhook — Razorpay event envelope.
 *
 * We validate only the fields we actually read. Razorpay includes many more,
 * but strict validation would break on every new field they add. `.passthrough()`
 * keeps unknowns, strict-typing the ones we depend on.
 */
export const RazorpayWebhookBody = z
  .object({
    event: z.string().min(1),
    /**
     * Unix-second timestamp Razorpay sets when the event was emitted. Used
     * by the worker's replay-window guard (rejects events older than 5 min).
     * Optional in the schema because we want malformed/missing-timestamp
     * deliveries to fall through to a clear `invalid request body` error
     * elsewhere if the rest of the payload is bad — the timestamp guard
     * itself rejects missing values explicitly.
     */
    created_at: z.number().int().nonnegative().optional(),
    // Razorpay shapes `payload` differently depending on event:
    // - payment.captured  → { payment: { entity } }
    // - subscription.*    → { subscription: { entity }, payment?: { entity } }
    // Both keys are optional at this layer; handlers check the shape they need.
    payload: z
      .object({
        payment: z
          .object({
            entity: z
              .object({
                id: z.string().min(1),
                order_id: z.string().optional(),
                amount: z.number().int().nonnegative().optional(),
                currency: z.string().optional(),
              })
              .passthrough(),
          })
          .passthrough()
          .optional(),
        subscription: z
          .object({
            entity: z
              .object({
                id: z.string().min(1),
                plan_id: z.string().optional(),
                status: z.string().optional(),
                current_start: z.number().nullable().optional(),
                current_end: z.number().nullable().optional(),
                ended_at: z.number().nullable().optional(),
                notes: z.record(z.string(), z.string()).optional(),
              })
              .passthrough(),
          })
          .passthrough()
          .optional(),
      })
      .passthrough(),
  })
  .passthrough();
export type RazorpayWebhookBody = z.infer<typeof RazorpayWebhookBody>;

/** POST /v1/dashboard/keys — generate a new API key. */
export const KeyCreateBody = z.object({
  name: z
    .string()
    .min(1)
    .max(64)
    .regex(/^[a-zA-Z0-9 _.\-]+$/, "letters, digits, space, _ . - only"),
});
export type KeyCreateBody = z.infer<typeof KeyCreateBody>;

/**
 * Parse a body through a schema; returns either the validated value or a
 * structured error suitable for `c.json(err, 400)`. Kept separate from Hono
 * validator middleware so the handlers stay explicit about what they accept.
 */
export function parseBody<T>(
  schema: z.ZodSchema<T>,
  raw: unknown,
): { ok: true; value: T } | { ok: false; error: { error: string; issues: unknown } } {
  const parsed = schema.safeParse(raw);
  if (parsed.success) return { ok: true, value: parsed.data };
  return {
    ok: false,
    error: {
      error: "invalid request body",
      issues: parsed.error.issues.map((i) => ({
        path: i.path.join("."),
        message: i.message,
      })),
    },
  };
}
