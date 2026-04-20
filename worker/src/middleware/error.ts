import type { Context } from "hono";

/** Global error handler — returns structured JSON errors. */
export function errorHandler(err: Error, c: Context): Response {
  console.error("Unhandled error:", err.message, err.stack);
  return c.json(
    {
      error: "Internal server error",
      message:
        c.env?.ENVIRONMENT === "production"
          ? "Something went wrong"
          : err.message,
    },
    500,
  );
}
