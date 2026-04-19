# ADR-000: Symbol-First Architecture

**Status:** Accepted
**Date:** 2026-04-19

## Context

Code intelligence for LLM agents can be built embedding-first (RAG over chunks) or structure-first (tree-sitter symbols + graph). Published evidence strongly favors structure:

- Aider's repo-map (tree-sitter + PageRank, no embeddings) achieved 26.3% on SWE-Bench Lite and 70.3% gold-file hit rate.
- SWE-Bench ablation: BM25 retrieval gave 1.96% solve rate vs 4.8% oracle — a 2.4x quality gap from better retrieval, not better generation.
- grepai benchmark: 97% fresh-input token reduction vs Claude Code's built-in tools.

Embedding-based search collapses on short exact queries (`findUserById`, `CONFIG_TIMEOUT`) and string-literal references (SQL, i18n keys).

## Decision

Recon uses a **symbol-first, hybrid architecture** with five tiers:

| Tier | Backend | Default? |
|------|---------|----------|
| T0 — Symbol exact | SQLite btree | Yes |
| T1 — Symbol fuzzy | SQLite FTS5 trigram + nucleo | Yes |
| T2 — Structured BM25 | Tantivy with CodeTokenizer | Yes |
| T3 — Raw text/regex | fff-grep (memmap + SIMD) | Yes |
| T4 — Semantic | LanceDB + jina-v2-base-code | Feature-gated |

Embeddings are the **fallback**, not the primary path.

## Consequences

- Tree-sitter parsing is a hard dependency — 9 grammars bundled at build time.
- The PageRank repo-map (`code_repo_map`) is the highest signal-per-token tool.
- Token reduction comes from structure (skeleton/outline), not from smarter retrieval alone.
- Embedding quality only matters for the long tail of natural-language queries.
