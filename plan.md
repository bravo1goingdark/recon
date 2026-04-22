# Recon — Remaining Implementation Plan

> Generated: 2026-04-22
> Status: Phase 1 (HIGH) and Phase 2 (MEDIUM) partially complete.
> Last commit: `7369a6e feat: populate incomplete tool responses and add missing tests`

---

## Progress Summary

| Phase | Items | Done | Remaining |
|-------|-------|------|-----------|
| HIGH (code discipline violations) | 5 | 5 | 0 |
| MEDIUM (incomplete features + missing tests) | 16 | 9 | 7 |
| LOW (optimizations + minor issues) | 19 | 0 | 19 |
| **Total** | **40** | **14** | **26** |

---

## Phase 3 — Incomplete Features (MEDIUM)

### 3.1 Filter DSL: `git_modified_only`, `size:`, `mtime:` constraints
**Priority:** HIGH within remaining | **Estimated effort:** 1–2 days

**Current state:** `ParsedFilter` has fields for `git_modified_only`, but `apply_filter()` never enforces it. `size:` and `mtime:` constraints are parsed by `fff-query-parser` but silently skipped.

**What to implement:**
- `apply_filter()` in `crates/recon-search/src/filters.rs`:
  - `git_modified_only` → resolve via `gix status --porcelain`, intersect with path list
  - `size:<50kb` / `size:>1mb` → `std::fs::metadata(path).len()` check
  - `mtime:>2d` / `mtime:<1h` → `metadata.modified()` vs `SystemTime::now()` check
- Add `sym:` constraint (our extension) → filter by symbol kind in the symbol table before text scan
- Add tests for combined constraints (extension + glob + git_modified)

**Files:** `crates/recon-search/src/filters.rs`

---

### 3.2 `multi_search` aho-corasick single-pass optimization
**Priority:** MEDIUM | **Estimated effort:** 0.5 day

**Current state:** Both `GrepBackend` and `FffBackend` iterate patterns sequentially — O(N×M) file reads for N patterns over M files.

**What to implement:**
- Use `aho_corasick::AhoCorasick` (already in workspace deps) to build a multi-pattern matcher
- Single pass over each file, collect hits tagged by pattern index
- Return `HashMap<pattern, Vec<TextHit>>` as today, but with O(M) file reads

**Files:** `crates/recon-search/src/text.rs`, `crates/recon-search/src/fff_backend.rs`

---

### 3.3 `FffBackend::refresh()` mmap cache invalidation
**Priority:** LOW | **Estimated effort:** 0.5 day

**Current state:** `refresh()` is a no-op. Current design mmaps per-query so there's nothing to invalidate — acceptable but doesn't match the FFF_INTEGRATION.md spec.

**What to implement:**
- Add `ArcSwap<HashMap<PathBuf, Arc<Mmap>>>` cache to `FffBackend`
- `search()` checks cache first, mmaps on miss
- `refresh(changed_paths)` removes entries from the cache for changed paths
- Benchmark: does caching actually help? For warm queries on the same files, yes.

**Files:** `crates/recon-search/src/fff_backend.rs`

---

### 3.4 Embedding tier connected to MCP tools
**Priority:** MEDIUM | **Estimated effort:** 1 day

**Current state:** `code_search(mode="semantic")` exists but the LanceDB/vector store path is feature-gated and not wired into the server's default search dispatch. The `embed` feature compiles but requires manual `init_embed()` call.

**What to implement:**
- Wire `mode="semantic"` into the non-feature-gated path as a graceful "embeddings not initialized" error
- Add `hybrid` mode to fuse Tantivy BM25 + vector results via RRF (already partially implemented for Tantivy+text)
- Add `code_search` integration test for semantic mode
- Document the `--features embed` requirement in README

**Files:** `crates/recon-server/src/server.rs`, `crates/recon-cli/src/main.rs`

---

### 3.5 Merkle tree integrated into indexing pipeline
**Priority:** MEDIUM | **Estimated effort:** 1 day

**Current state:** `MerkleSnapshot` exists with build/diff/save/load but is **never used** by `index_repo_incremental`. The incremental path relies entirely on gix. No Merkle-based fallback for non-git repos.

**What to implement:**
- On cold start: build `MerkleSnapshot` from all files, diff against saved snapshot (if exists)
- Changed files from Merkle diff → feed into `index_diff()`
- Save snapshot after indexing completes
- Non-git repos: Merkle diff is the **only** incremental mechanism

**Files:** `crates/recon-indexer/src/indexer.rs`, `crates/recon-indexer/src/merkle.rs`

---

## Phase 4 — Missing Test Coverage (MEDIUM)

### 4.1 Watcher tests
**Priority:** MEDIUM | **Estimated effort:** 0.5 day

**Current state:** `crates/recon-indexer/src/watcher.rs` has zero tests.

**What to implement:**
- Basic `Watcher::new` + `try_recv` smoke test
- Write a file in a temp dir, assert `try_recv` returns the path within debounce window
- Test `.recon` directory filtering

**Files:** `crates/recon-indexer/src/watcher.rs`

---

### 4.2 E2E integration tests against real OSS repos
**Priority:** MEDIUM | **Estimated effort:** 2–3 days

**Current state:** Per CLAUDE.md, integration tests against real OSS repos should live in `/tests/e2e/` as git submodules. This directory does not exist.

**What to implement:**
- Create `tests/e2e/` directory
- Add 2–3 small OSS repos as git submodules (e.g., `serde`, `tiny_http`, a small Python project)
- Write harness: index each repo → run fixed tool calls → assert against hand-authored YAML ground truth
- Tools to test: `code_find_symbol`, `code_search`, `code_outline`, `code_repo_map`, `code_list`
- Add to CI workflow (exclude from PR if submodules not available)

**Files:** `tests/e2e/`, `.github/workflows/ci.yml`

---

### 4.3 `code_read_symbol` / `code_find_refs` tool implementation tests
**Priority:** MEDIUM | **Estimated effort:** 0.5 day

**Current state:** `crates/recon-server/src/server.rs` has only 4 trivial tests (empty-repo cases). No tests for actual indexed content.

**What to implement:**
- Index a temp repo with known symbols
- Test `code_read_symbol` returns correct body, parent_chain, callers, callees
- Test `code_find_refs` returns correct line numbers and enclosing symbols
- Test `code_search` with exact, regex, and hybrid modes
- Test `code_reindex` with `force: true`

**Files:** `crates/recon-server/src/server.rs` (test module)

---

## Phase 5 — Optimizations (LOW)

### 5.1 `redact_secrets` O(n²) → O(n) single-pass
**Priority:** LOW | **Estimated effort:** 0.5 day

**Current state:** Repeated `replace_range` on `String` shifts all subsequent bytes — quadratic with many secrets.

**What to implement:**
- Collect all replacement ranges first
- Build output `String` in one pass with pre-allocated capacity
- Benchmark before/after on a file with 100+ secrets

**Files:** `crates/recon-core/src/redact.rs`

---

### 5.2 Batch inserts: multi-row `INSERT INTO ... VALUES (...), (...)`
**Priority:** LOW | **Estimated effort:** 0.5 day

**Current state:** `upsert_symbols_batch` calls `stmt.execute()` per symbol inside a transaction.

**What to implement:**
- Build multi-row VALUES clause for batches of 100–500 symbols
- SQLite supports up to ~32K parameters per statement; 500 symbols × ~12 columns = 6K params (safe)
- Benchmark: expected 2–5× speedup for large batch inserts

**Files:** `crates/recon-storage/src/store.rs`

---

### 5.3 `row_to_symbol` avoid `Vec<u8>` for fixed 32-byte body_hash
**Priority:** LOW | **Estimated effort:** 0.25 day

**Current state:** `row.get(12)` allocates a `Vec<u8>` for a known 32-byte blob.

**What to implement:**
- Use `rusqlite::types::ValueRef` to read the blob directly into `[u8; 32]`
- Same fix in `get_file_hash` and `read_fns`

**Files:** `crates/recon-storage/src/store.rs`, `crates/recon-storage/src/read_fns.rs`

---

### 5.4 `f32_to_le_bytes` use `bytemuck::cast_slice`
**Priority:** LOW | **Estimated effort:** 0.25 day

**Current state:** `flat_map` + `collect` allocates a new `Vec<u8>` per embedding (3072 bytes).

**What to implement:**
- Add `bytemuck` to workspace deps
- Replace with `bytemuck::cast_slice::<f32, u8>(v)` — zero-copy `&[u8]`
- Caller (`rusqlite::params!`) accepts `&[u8]`

**Files:** `crates/recon-embed/src/vector_store.rs`

---

### 5.5 `EmbedService` bounded channel with backpressure
**Priority:** LOW | **Estimated effort:** 0.25 day

**Current state:** Uses `unbounded()` channel — under heavy load (full repo re-index) could grow without bound.

**What to implement:**
- Replace with `crossbeam_channel::bounded(1024)` or similar
- Sender blocks when full — natural backpressure
- Add timeout or error on send failure

**Files:** `crates/recon-embed/src/embed_service.rs`

---

### 5.6 `code_search` avoid `all_file_paths()` before mode dispatch
**Priority:** LOW | **Estimated effort:** 0.25 day

**Current state:** `all_file_paths()` called unconditionally at the top of `code_search`, even for semantic mode.

**What to implement:**
- Move `all_file_paths()` call inside the branches that need it (exact, regex, hybrid)
- Semantic mode doesn't need file paths

**Files:** `crates/recon-server/src/server.rs`

---

### 5.7 `code_skeleton` skip file read when symbols exist
**Priority:** LOW | **Estimated effort:** 0.25 day

**Current state:** Reads entire file via `tokio::fs::read_to_string` even when symbols exist in the index.

**What to implement:**
- Check symbols first; only read file if skeleton is empty
- Or: build skeleton from indexed symbol signatures + docs, skip file entirely

**Files:** `crates/recon-server/src/server.rs`

---

### 5.8 Rename `upsert_symbol` / `upsert_refs` to `insert_symbol` / `insert_refs`
**Priority:** LOW | **Estimated effort:** 0.25 day

**Current state:** Both use plain `INSERT` without `ON CONFLICT` — true inserts, not upserts.

**What to implement:**
- Rename functions to match actual behavior
- Or: add `ON CONFLICT DO NOTHING` / `ON CONFLICT DO UPDATE` to make them true upserts

**Files:** `crates/recon-storage/src/store.rs`

---

### 5.9 `PRAGMA optimize` at close time via `Drop`
**Priority:** LOW | **Estimated effort:** 0.25 day

**Current state:** `PRAGMA optimize` called at open time (limited effect).

**What to implement:**
- Add `Drop` impl for `Store` that runs `PRAGMA optimize`
- Remove from `Store::init`

**Files:** `crates/recon-storage/src/store.rs`

---

### 5.10 `serve_http` bounded concurrency
**Priority:** LOW | **Estimated effort:** 0.25 day

**Current state:** `tokio::spawn` per connection with no limit — unbounded under heavy load.

**What to implement:**
- Use `tokio::task::JoinSet` with a max concurrency limit (e.g., 100)
- Or: semaphore-based rate limiting

**Files:** `crates/recon-cli/src/main.rs`

---

### 5.11 `NO_COLOR` support in CLI output
**Priority:** LOW | **Estimated effort:** 0.25 day

**Current state:** ANSI color codes emitted unconditionally.

**What to implement:**
- Check `NO_COLOR` env var (per https://no-color.org/)
- Or: add `supports-color` / `anstream` crate for automatic detection
- Strip ANSI codes when piped or `NO_COLOR=1`

**Files:** `crates/recon-cli/src/pretty.rs`

---

### 5.12 `is_generated_content` scan full buffer, not line-by-line
**Priority:** LOW | **Estimated effort:** 0.25 day

**Current state:** Aho-Corasick applied per-line.

**What to implement:**
- Apply to first N bytes of content buffer at once
- Faster single-pass scan

**Files:** `crates/recon-indexer/src/walker.rs`

---

### 5.13 Fix MerkleSnapshot docs vs implementation mismatch
**Priority:** LOW | **Estimated effort:** 0.25 day

**Current state:** Doc comment says "Directory nodes are blake3 hashes of their sorted children's hashes" but only leaf hashes are stored.

**What to implement:**
- Either: implement hierarchical tree (bubble hashes up)
- Or: fix docs to match flat snapshot reality

**Files:** `crates/recon-indexer/src/merkle.rs`

---

### 5.14 CI: include `recon-embed` in test/clippy jobs
**Priority:** LOW | **Estimated effort:** 0.5 day

**Current state:** Both test and clippy jobs exclude `recon-embed` due to disk constraints.

**What to implement:**
- Use a larger runner for embed tests (e.g., `ubuntu-latest-large`)
- Or: run embed tests on a schedule (nightly) rather than every PR
- At minimum: add a `cargo check -p recon-embed` step

**Files:** `.github/workflows/ci.yml`

---

## Phase 6 — Out of Scope for v0.1 (Deferred)

Per `docs/IMPLEMENTATION.md` §15, these are planned for v1/production but must **not** be started in v0.1:

- SCIP consumption (tier-5 precision)
- Live LSP delegation
- Wasm-runtime grammar loading for niche languages
- Embedding model enabled by default (feature flag scaffolded, disabled)
- OTLP tracing (Prometheus metrics only for now)
- Team-shared indexes (Cursor-style simhash reuse)
- Code-editing tools (this server is read-only)
- Windows as a primary target (builds, but CI green is the bar)

---

## Effort Summary

| Phase | Items | Est. Effort |
|-------|-------|-------------|
| 3 — Incomplete Features | 5 | 4–5.5 days |
| 4 — Missing Tests | 3 | 3–4 days |
| 5 — Optimizations | 12 | 3.5–4.5 days |
| **Total remaining** | **26** | **10.5–14 days** |

---

## Recommended Order

1. **Phase 3.1** (Filter DSL) — unlocks useful search for all tools
2. **Phase 4.3** (Server tool tests) — validates the fixes from Phase 2
3. **Phase 3.4** (Embedding wiring) — completes the semantic search tier
4. **Phase 3.5** (Merkle integration) — enables non-git incremental indexing
5. **Phase 4.1** (Watcher tests) — covers the last untested module
6. **Phase 5.1–5.14** (Optimizations) — batch these together, 1–2 per session
7. **Phase 3.2** (multi_search aho-corasick) — performance win for refactor workflows
8. **Phase 3.3** (FffBackend refresh) — spec compliance
9. **Phase 4.2** (E2E integration tests) — highest effort, do last
