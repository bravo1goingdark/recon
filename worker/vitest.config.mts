import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

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
export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.toml" },
      miniflare: {
        d1Databases: ["RECON_DB"],
        // EMBED_CACHE is the chunk-vector cache used by /v1/embed. Tests
        // that don't touch embed run unaffected; the embed test suite
        // pre-populates entries via env.EMBED_CACHE.put before issuing
        // requests, then asserts cache-hit behavior.
        kvNamespaces: ["EMBED_CACHE"],
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
          // /v1/embed forwards uncached batches here. Tests use a sentinel
          // host that triggers the route's network-error path (503) unless
          // they install a fetch mock first. The MODAL_AUTH_TOKEN is the
          // shape the route expects in the outbound Authorization header.
          MODAL_EMBED_URL: "https://modal.test/embed",
          MODAL_AUTH_TOKEN: "test-modal-bearer",
        },
      },
    }),
  ],
  test: {
    globals: true,
    coverage: {
      provider: "istanbul",
    },
  },
});
