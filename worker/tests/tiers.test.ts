/**
 * Tier + currency helpers unit tests.
 *
 * Pure runtime behavior — no D1, no worker fetch. Pins the per-currency
 * price shape so a careless edit to tiers.ts can't silently drop the
 * INR leg (which would route Indian users to a non-UPI-compatible
 * USD subscription).
 */

import { describe, expect, it } from "vitest";
import {
  getTierConfig,
  getTierPrice,
  purchasableTiers,
} from "../src/lib/tiers";

describe("getTierConfig", () => {
  it("returns Pro for case-insensitive 'pro'", () => {
    expect(getTierConfig("pro").name).toBe("Pro");
    expect(getTierConfig("PRO").name).toBe("Pro");
    expect(getTierConfig("Pro").name).toBe("Pro");
  });

  it("falls back to Free for unknown tier names", () => {
    expect(getTierConfig("enterprise-gold").name).toBe("Free");
    expect(getTierConfig("").name).toBe("Free");
  });
});

describe("getTierPrice — every paid tier has both INR and USD", () => {
  it("Pro INR = ₹249, USD = $3", () => {
    const inr = getTierPrice("Pro", "INR");
    const usd = getTierPrice("Pro", "USD");
    expect(inr).not.toBeNull();
    expect(usd).not.toBeNull();
    expect(inr?.amount).toBe(24900); // paise
    expect(usd?.amount).toBe(300); // cents
    expect(inr?.display).toBe("₹249/mo");
    expect(usd?.display).toBe("$3/mo");
  });

  it("Team INR = ₹599, USD = $7", () => {
    expect(getTierPrice("Team", "INR")?.amount).toBe(59900);
    expect(getTierPrice("Team", "USD")?.amount).toBe(700);
    expect(getTierPrice("Team", "INR")?.display).toBe("₹599/mo");
    expect(getTierPrice("Team", "USD")?.display).toBe("$7/mo");
  });

  it("Free has no price in either currency (unpurchasable)", () => {
    expect(getTierPrice("Free", "INR")).toBeNull();
    expect(getTierPrice("Free", "USD")).toBeNull();
  });

  it("Enterprise has no price in either currency (contact-sales)", () => {
    expect(getTierPrice("Enterprise", "INR")).toBeNull();
    expect(getTierPrice("Enterprise", "USD")).toBeNull();
  });
});

describe("purchasableTiers", () => {
  it("returns Pro and Team only", () => {
    const names = purchasableTiers().map((t) => t.name);
    expect(names).toContain("Pro");
    expect(names).toContain("Team");
    expect(names).not.toContain("Free");
    expect(names).not.toContain("Enterprise");
    expect(names.length).toBe(2);
  });
});
