/**
 * Shared test harness.
 *
 * Every test that touches D1 runs against a fresh schema (migrations 0001
 * + 0002 applied). `resetDb()` wipes and re-applies between tests so order
 * dependencies can't hide correctness bugs.
 *
 * `fetchApp(path, init)` sends a request through the built worker via
 * the SELF.fetch magic binding from @cloudflare/vitest-pool-workers —
 * the handler runs in workerd with real D1, so tests exercise the same
 * code path production does.
 *
 * Migration SQL is imported via Vite's `?raw` query because node:fs is
 * not available inside workerd; Vite inlines the file content at build
 * time so no filesystem access is needed at runtime.
 */

import { applyD1Migrations, env, SELF } from "cloudflare:test";
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-expect-error vite `?raw` import — string, no type declaration
import migration0001 from "../migrations/0001_initial.sql?raw";
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-expect-error vite `?raw` import — string, no type declaration
import migration0002 from "../migrations/0002_payment_events.sql?raw";
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-expect-error vite `?raw` import — string, no type declaration
import migration0003 from "../migrations/0003_subscription_lifecycle.sql?raw";
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-expect-error vite `?raw` import — string, no type declaration
import migration0004 from "../migrations/0004_currency_per_plan.sql?raw";
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-expect-error vite `?raw` import — string, no type declaration
import migration0005 from "../migrations/0005_user_repos.sql?raw";
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-expect-error vite `?raw` import — string, no type declaration
import migration0006 from "../migrations/0006_webhook_events_dropped.sql?raw";
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-expect-error vite `?raw` import — string, no type declaration
import migration0007 from "../migrations/0007_webhook_events_seen.sql?raw";
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-expect-error vite `?raw` import — string, no type declaration
import migration0008 from "../migrations/0008_subscription_currency.sql?raw";
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-expect-error vite `?raw` import — string, no type declaration
import migration0009 from "../migrations/0009_usage_rollups.sql?raw";
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-expect-error vite `?raw` import — string, no type declaration
import migration0010 from "../migrations/0010_usage_rollups_per_repo.sql?raw";
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-expect-error vite `?raw` import — string, no type declaration
import migration0011 from "../migrations/0011_usage_rollups_measured.sql?raw";
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-expect-error vite `?raw` import — string, no type declaration
import migration0012 from "../migrations/0012_usage_rollups_latency_saved.sql?raw";

/**
 * Split a SQL file into individual statements.
 * Naive but sufficient for our migrations: strip `-- …` line comments,
 * split on `;`, trim, drop empties. We don't use multi-statement
 * procedures or nested semicolons.
 */
function splitSql(sql: string): string[] {
  return sql
    .split("\n")
    .filter((line) => !line.trim().startsWith("--"))
    .join("\n")
    .split(";")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

const MIGRATIONS = [
  { name: "0001_initial", queries: splitSql(migration0001 as string) },
  { name: "0002_payment_events", queries: splitSql(migration0002 as string) },
  {
    name: "0003_subscription_lifecycle",
    queries: splitSql(migration0003 as string),
  },
  {
    name: "0004_currency_per_plan",
    queries: splitSql(migration0004 as string),
  },
  {
    name: "0005_user_repos",
    queries: splitSql(migration0005 as string),
  },
  {
    name: "0006_webhook_events_dropped",
    queries: splitSql(migration0006 as string),
  },
  {
    name: "0007_webhook_events_seen",
    queries: splitSql(migration0007 as string),
  },
  {
    name: "0008_subscription_currency",
    queries: splitSql(migration0008 as string),
  },
  {
    name: "0009_usage_rollups",
    queries: splitSql(migration0009 as string),
  },
  {
    name: "0010_usage_rollups_per_repo",
    queries: splitSql(migration0010 as string),
  },
  {
    name: "0011_usage_rollups_measured",
    queries: splitSql(migration0011 as string),
  },
  {
    name: "0012_usage_rollups_latency_saved",
    queries: splitSql(migration0012 as string),
  },
];

/**
 * Apply all migrations to a fresh D1 database.
 * Call from beforeEach() to keep tests hermetic.
 */
export async function resetDb(): Promise<void> {
  const db = (env as { RECON_DB: D1Database }).RECON_DB;
  // Drop every table we created so applyD1Migrations re-runs cleanly.
  const tables = [
    "webhook_events_seen",
    "webhook_events_dropped",
    "subscription_plans",
    "payment_events",
    "subscriptions",
    "payments",
    "sessions",
    "usage_rollups",
    "user_repos",
    "api_keys",
    "users",
    "d1_migrations",
  ];
  for (const t of tables) {
    await db.prepare(`DROP TABLE IF EXISTS ${t}`).run();
  }
  await applyD1Migrations(db, MIGRATIONS);
}

export { env, SELF };

/** Convenience: hit the worker and parse the JSON body. */
export async function getJson(
  path: string,
  init?: RequestInit,
): Promise<{ status: number; body: unknown; headers: Headers }> {
  const resp = await SELF.fetch("http://localhost" + path, init);
  const text = await resp.text();
  let body: unknown = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = text;
  }
  return { status: resp.status, body, headers: resp.headers };
}
