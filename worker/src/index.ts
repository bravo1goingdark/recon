import { Hono } from "hono";
import { cors } from "./middleware/cors";
import { errorHandler } from "./middleware/error";
import { licenseRoutes } from "./routes/license";
import { authRoutes } from "./routes/auth";
import { dashboardRoutes } from "./routes/dashboard";
import { billingRoutes } from "./routes/billing";
import type { Env } from "./types";

const app = new Hono<{ Bindings: Env }>();

// CORS before all routes (required for cross-origin dashboard calls)
app.use("*", cors);
app.onError(errorHandler);

// API routes
app.route("/v1/license", licenseRoutes);
app.route("/v1/auth", authRoutes);
app.route("/v1/dashboard", dashboardRoutes);
app.route("/v1/billing", billingRoutes);

// Health check
app.get("/health", (c) => c.json({ status: "ok", version: "1.0.0" }));

// 404 fallback
app.all("*", (c) => c.json({ error: "Not found" }, 404));

export default app;
