/**
 * Zod schemas for every externally-facing POST body.
 *
 * Every route that accepts user or Razorpay input validates through one of
 * these schemas before touching D1. A failed validation returns a structured
 * 400 — we never let malformed bodies reach the business logic.
 */

import { z } from "zod";

/** POST /v1/billing/checkout — purchase a tier. */
export const CheckoutBody = z.object({
  /** Canonical tier name: "Free", "Pro", or "Team". Free is rejected at the handler. */
  tier: z.enum(["Free", "Pro", "Team"]),
});
export type CheckoutBody = z.infer<typeof CheckoutBody>;

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
    payload: z
      .object({
        payment: z
          .object({
            entity: z
              .object({
                id: z.string().min(1),
                order_id: z.string().min(1),
                amount: z.number().int().nonnegative().optional(),
                currency: z.string().optional(),
              })
              .passthrough(),
          })
          .passthrough(),
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
