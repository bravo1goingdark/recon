/**
 * Tier definitions — must exactly mirror the Rust constants in
 * crates/recon-server/src/router.rs TierLimits (lines 33-68).
 *
 * Dual-currency pricing: every paid tier carries an INR and a USD price.
 * Razorpay plans are currency-specific (one plan per (tier, currency) pair);
 * the caller chooses which to use via the currency parameter on
 * `/v1/billing/subscribe`. Indian users default to INR so they get the
 * full UPI / Net Banking eNACH / RuPay payment menu; everyone else
 * defaults to USD. See src/routes/billing.ts for the defaulting logic.
 */

export interface TierLimits {
  max_repos: number;
  max_files: number;
  max_loc: number;
}

/** Supported Razorpay subscription currencies. */
export type Currency = "INR" | "USD";

export interface TierPrice {
  /** Amount in the smallest unit of the currency (paise for INR, cents for USD). */
  amount: number;
  /** Display string shown in UI. */
  display: string;
}

export interface TierConfig {
  name: string;
  limits: TierLimits;
  /**
   * Currency-specific prices. Free and Enterprise tiers get empty price
   * objects because they're not purchased through /subscribe.
   */
  prices: Partial<Record<Currency, TierPrice>>;
}

/** Must match TierLimits::FREE in router.rs */
const FREE: TierConfig = {
  name: "Free",
  limits: { max_repos: 1, max_files: 250, max_loc: 10_000 },
  prices: {}, // not purchasable
};

/** Must match TierLimits::PRO in router.rs */
const PRO: TierConfig = {
  name: "Pro",
  limits: { max_repos: 10, max_files: 5_000, max_loc: 200_000 },
  prices: {
    // ₹249/mo in paise. Slightly softened from a direct-FX ~₹255 → 249
    // reads better and is consistent with indie dev-tool pricing in India.
    INR: { amount: 24900, display: "₹249/mo" },
    USD: { amount: 300, display: "$3/mo" },
  },
};

/** Must match TierLimits::TEAM in router.rs */
const TEAM: TierConfig = {
  name: "Team",
  limits: { max_repos: 25, max_files: 50_000, max_loc: 4_000_000 },
  prices: {
    INR: { amount: 59900, display: "₹599/mo" },
    USD: { amount: 700, display: "$7/mo" },
  },
};

/** Must match TierLimits::ENTERPRISE in router.rs */
const ENTERPRISE: TierConfig = {
  name: "Enterprise",
  limits: {
    max_repos: 1_000,
    max_files: Number.MAX_SAFE_INTEGER,
    max_loc: Number.MAX_SAFE_INTEGER,
  },
  prices: {}, // contact-sales
};

export const TIERS: Record<string, TierConfig> = {
  Free: FREE,
  Pro: PRO,
  Team: TEAM,
  Enterprise: ENTERPRISE,
};

/** Get tier config by name (case-insensitive), defaults to Free. */
export function getTierConfig(name: string): TierConfig {
  const normalized =
    name.charAt(0).toUpperCase() + name.slice(1).toLowerCase();
  return TIERS[normalized] ?? FREE;
}

/**
 * Get the price for a (tier, currency) pair. Returns null when the tier
 * has no price in that currency — callers should 400 on that path.
 */
export function getTierPrice(
  tierName: string,
  currency: Currency,
): TierPrice | null {
  return getTierConfig(tierName).prices[currency] ?? null;
}

/** All purchasable tiers (excludes Free and Enterprise/contact-sales). */
export function purchasableTiers(): TierConfig[] {
  return Object.values(TIERS).filter((t) => Object.keys(t.prices).length > 0);
}
