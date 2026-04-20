import { hmacSha256Hex, timingSafeEqual } from "./crypto";

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
  const resp = await fetch("https://api.razorpay.com/v1/orders", {
    method: "POST",
    headers: {
      Authorization: "Basic " + btoa(`${keyId}:${keySecret}`),
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ amount, currency, receipt, notes }),
  });

  if (!resp.ok) {
    const err = await resp.text();
    throw new Error(`Razorpay order failed: ${resp.status} ${err}`);
  }

  return (await resp.json()) as RazorpayOrder;
}

/** Verify Razorpay webhook signature (HMAC-SHA256). */
export async function verifyWebhookSignature(
  body: string,
  signature: string,
  secret: string,
): Promise<boolean> {
  const expected = await hmacSha256Hex(secret, body);
  return timingSafeEqual(expected, signature);
}
