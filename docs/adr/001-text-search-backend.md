# ADR-001: Text Search Backend

**Status:** Accepted  
**Date:** 2026-04-19

## Context

The search layer needs a fast text/regex search backend for `code_search`, `code_find_strings`, and `code_multi_find`. The FFF_INTEGRATION.md plan specifies `fff-grep` behind a `TextSearcher` trait seam, with the existing ripgrep `grep-*` crates kept as fallback.

## Decision

- **`TextSearcher` trait** defined in `recon-search::search_trait` with `search`, `multi_search`, and `refresh` methods.
- **`GrepBackend`** wraps ripgrep's `grep-matcher`/`grep-regex`/`grep-searcher` — file-based search, no caching.
- **`FffBackend`** wraps `fff-grep =0.4.0` — memory-maps files via `memmap2` and calls `search_slice`. Pinned to exact stable version per FFF_INTEGRATION.md pin policy.
- **`fff-query-parser =0.4.0`** powers the `filter` DSL parameter on search tools, parsing constraints like `*.rs`, `type:rust`, `status:modified`, `!test`.
- Free functions `search_files`/`search_file` preserved for backward compatibility.

## Rationale

- The trait seam allows swapping backends without touching the 12 MCP tool handlers.
- fff-grep 0.4.0 only supports `search_slice(&[u8])`, not file paths — we mmap files ourselves.
- Pinning to `=0.4.0` avoids nightly churn while fff stabilizes toward 1.0.
- The `GrepBackend` stays as fallback if fff's API breaks at 0.5+.

## Consequences

- Two search backends in the binary (small size cost, ~2 MB).
- Server currently uses `GrepBackend` via the free functions; switching to `FffBackend` as default is a one-line change.
- Future: when fff ships aho-corasick multi-pattern in a stable release, `FffBackend::multi_search` can be upgraded to true single-pass.
