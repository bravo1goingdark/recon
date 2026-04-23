/**
 * Tier definitions — must exactly mirror the Rust constants in
 * crates/recon-server/src/router.rs TierLimits (lines 33-68).
 */

export interface TierLimits {
  max_repos: number;
  max_files: number;
  max_loc: number;
}

export interface TierConfig {
  name: string;
  limits: TierLimits;
  /** Monthly price in USD cents. 0 = free, -1 = contact sales. */
  price_cents: number;
  /** Display price string shown in UI. */
  price_display: string;
}

/** Must match TierLimits::FREE in router.rs */
const FREE: TierConfig = {
  name: "Free",
  limits: { max_repos: 1, max_files: 250, max_loc: 5_000 },
  price_cents: 0,
  price_display: "Free",
};

/** Must match TierLimits::PRO in router.rs */
const PRO: TierConfig = {
  name: "Pro",
  limits: { max_repos: 10, max_files: 5_000, max_loc: 200_000 },
  price_cents: 300, // $3.00 USD
  price_display: "$3/mo",
};

/** Must match TierLimits::TEAM in router.rs */
const TEAM: TierConfig = {
  name: "Team",
  limits: { max_repos: 25, max_files: 50_000, max_loc: 4_000_000 },
  price_cents: 700, // $7.00 USD
  price_display: "$7/mo",
};

/** Must match TierLimits::ENTERPRISE in router.rs */
const ENTERPRISE: TierConfig = {
  name: "Enterprise",
  limits: {
    max_repos: 1_000,
    max_files: Number.MAX_SAFE_INTEGER,
    max_loc: Number.MAX_SAFE_INTEGER,
  },
  price_cents: -1,
  price_display: "Contact us",
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

/** All purchasable tiers (excludes Free and Enterprise/contact-sales). */
export function purchasableTiers(): TierConfig[] {
  return Object.values(TIERS).filter((t) => t.price_cents > 0);
}
