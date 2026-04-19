# ADR-002: Output Shape Discipline

**Status:** Accepted
**Date:** 2026-04-19

## Context

MCP tool responses are consumed by an LLM planner that makes tool-selection decisions based on descriptions and prior responses. Free-form text responses force the model to parse heterogeneous output, waste tokens on formatting variation, and make description-based tool search less effective.

Claude Code truncates tool descriptions at 2 KB. With Anthropic's tool-search BM25 ranking, distinctive vocabulary in descriptions matters more than prose.

## Decision

Every tool returns exactly one of five canonical shapes defined in `recon-core::shapes::ToolOutput`:

| Shape | Purpose | Typical tokens |
|-------|---------|---------------|
| `Outline` | One-line-per-symbol tree view | 300-500 |
| `Skeleton` | Signatures + docs, bodies elided | 200-400 |
| `SymbolCard` | Full source + parent chain + callers | 200-800 |
| `ReferenceDigest` | Count + top-k call sites as path:line | 100-300 |
| `Diagnostics` | file:line:col: message format | 50-200 |

All shapes include a `token_estimate` field computed via tiktoken-rs cl100k_base.

## Consequences

- Tool descriptions stay under 2 KB by naming the output shape instead of describing format.
- The model can predict response size before calling a tool.
- Adding a new tool requires choosing an existing shape — new shapes need an ADR.
- No free-form text ever leaves the server.
