# ADR-003: Stdio Transport Hygiene

**Status:** Accepted
**Date:** 2026-04-19

## Context

MCP over stdio uses stdout as the JSON-RPC channel. Any stray output to stdout — a `println!` in library code, a logger default, a dependency's banner — silently corrupts the protocol and breaks the connection. This is the #1 MCP server bug category (postman/mcp-server shipped broken for exactly this).

## Decision

1. **All logging goes to stderr.** `tracing_subscriber::fmt().with_writer(std::io::stderr)` is set in `recon-cli` before anything else runs.

2. **No `println!` anywhere.** Library crates use `tracing::info!` / `tracing::debug!` etc. The CLI binary uses `eprintln!` for user-facing output.

3. **Stdout hygiene test.** `crates/recon-cli/tests/stdout_hygiene.rs` starts the server as a subprocess, sends an MCP `initialize` request, and asserts every line on stdout is valid JSON-RPC. This test runs in CI on every commit.

4. **Dependencies audited.** Any new dependency that could write to stdout (loggers, progress bars, model download reporters) must be configured to use stderr or suppressed.

## Consequences

- The stdout hygiene test catches regressions before they reach users.
- fastembed's model download progress is configured via `with_show_download_progress(true)` which writes to stderr by default in ort 2.0.
- `tracing-subscriber` JSON formatter is the only structured output path.
- The `--http` transport mode (Streamable HTTP) does not have this constraint, but the discipline is maintained uniformly.
