import { Hono } from "hono";
import { cors } from "./middleware/cors";
import { errorHandler } from "./middleware/error";
import { licenseRoutes } from "./routes/license";
import { authRoutes } from "./routes/auth";
import { dashboardRoutes } from "./routes/dashboard";
import { billingRoutes } from "./routes/billing";
import { accountRoutes } from "./routes/account";
import { embedRoutes } from "./routes/embed";
import { handleScheduled } from "./scheduled";
import type { Env } from "./types";

const app = new Hono<{ Bindings: Env }>();

// CORS before all routes (required for cross-origin dashboard calls)
app.use("*", cors);
app.onError(errorHandler);

// API routes — served at /api/v1/* via Pages proxy
app.route("/api/v1/license", licenseRoutes);
app.route("/api/v1/auth", authRoutes);
app.route("/api/v1/dashboard", dashboardRoutes);
app.route("/api/v1/billing", billingRoutes);
app.route("/api/v1/account", accountRoutes);
app.route("/api/v1/embed", embedRoutes);

// Also mount without /api prefix for direct Worker access
app.route("/v1/license", licenseRoutes);
app.route("/v1/auth", authRoutes);
app.route("/v1/dashboard", dashboardRoutes);
app.route("/v1/billing", billingRoutes);
app.route("/v1/account", accountRoutes);
app.route("/v1/embed", embedRoutes);

// Health check — three aliases. `/v1/health` is what `recon doctor`
// hits; `/health` and `/api/health` are kept for the Pages proxy /
// uptime monitors that already point at them.
const health = (c: { json: (b: object) => Response }) =>
  c.json({ status: "ok", version: "1.0.0" });
app.get("/health", health);
app.get("/api/health", health);
app.get("/v1/health", health);
app.get("/api/v1/health", health);

// 404 fallback
app.all("*", (c) => c.json({ error: "Not found" }, 404));

// Module-format default export: fetch for HTTP, scheduled for cron triggers.
// The `scheduled` handler runs on the cadence defined in wrangler.toml
// [triggers] — currently hourly, for subscription-expiry downgrades.
export default {
  fetch: app.fetch,
  async scheduled(
    _controller: ScheduledController,
    env: Env,
    ctx: ExecutionContext,
  ) {
    ctx.waitUntil(handleScheduled(env));
  },
};
