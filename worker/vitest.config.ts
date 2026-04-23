import { defineWorkersConfig } from "@cloudflare/vitest-pool-workers/config";

/**
 * Worker test harness.
 *
 * @cloudflare/vitest-pool-workers runs tests inside workerd, so D1 / KV /
 * R2 bindings behave exactly as they will in production. No mocks, no
 * local-only shims that drift from reality.
 *
 * `miniflare.kvNamespaces` / `d1Databases` here shape the test environment
 * — the actual database schema is applied once per suite via the
 * `applyD1Migrations` helper in tests/setup.ts.
 *
 * Rate-limit bindings are NOT provisioned here. The middleware fails open
 * on missing bindings (dev/test pattern), so tests that don't care about
 * the limiter run without friction. The dedicated ratelimit test supplies
 * its own minimal bucket binding via an env override.
 */
export default defineWorkersConfig({
  test: {
    globals: true,
    poolOptions: {
      workers: {
        wrangler: { configPath: "./wrangler.toml" },
        miniflare: {
          d1Databases: ["RECON_DB"],
          compatibilityDate: "2025-04-01",
          compatibilityFlags: ["nodejs_compat"],
          bindings: {
            // Secrets — all set to deterministic test values. Real secrets
            // are never in the repo; these are just non-empty placeholders
            // so handlers that touch them don't crash at module-load.
            GITHUB_CLIENT_ID: "test-client-id",
            GITHUB_CLIENT_SECRET: "test-client-secret",
            RAZORPAY_KEY_ID: "rzp_test_abc",
            RAZORPAY_KEY_SECRET: "rzp_test_secret",
            // Webhook secret used by HMAC sig verification in tests.
            RAZORPAY_WEBHOOK_SECRET: "test-webhook-secret",
            SESSION_SIGNING_KEY: "test-session-key-thirty-two-bytes!",
            ALLOWED_ORIGINS: "http://localhost:8788",
            FRONTEND_URL: "http://localhost:8788",
            // Must match the CLI build's RECON_LICENSE_HMAC_KEY in integration;
            // tests just need any non-empty string for the Rust signing path.
            LICENSE_HMAC_SECRET: "test-license-hmac-secret",
          },
        },
      },
    },
  },
});
