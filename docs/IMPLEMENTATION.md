# code-intel — Implementation Plan for Claude Code

> **For Claude Code reading this file:** this document is your execution spec. Work through milestones in order. At each `CHECKPOINT` marker, stop and wait for human review before continuing. Every milestone has a Definition of Done — don't claim completion until all acceptance criteria are green. Before starting, read `CLAUDE.md` (create it from §2 if missing), then `docs/ARCHITECTURE.md` (the main architecture doc), then `docs/FFF_INTEGRATION.md` (the search-layer addendum).

**Project:** `code-intel` — a Rust MCP server that replaces Claude Code's `Read`, `Grep`, `Glob` with token-lean, symbol-aware equivalents.
**Target:** single static binary, sub-100 ms p99 query latency, <1 s file-save-to-queryable freshness, 5–10× token reduction on typical coding tasks, scale to million-LOC monorepos.
**Non-goals:** code editing (read-only server), remote indexing, GPU requirement, Windows-first.

---

## 0. How to use this document

**Workflow for each milestone:**

1. Claude Code reads the milestone section end-to-end.
2. Claude Code creates a task list using its TODO tool with one entry per task in that milestone.
3. Claude Code executes tasks in order, running tests after each.
4. Claude Code reports progress at the end of each task.
5. At the `CHECKPOINT` marker, Claude Code stops, summarizes what's done, and asks the human to review and approve before proceeding to the next milestone.

**Conventions:**

- Every file path is relative to the repo root.
- Every acceptance test is a real `cargo test` invocation; "green" means the named test passes.
- `CHECKPOINT(Mn)` = mandatory human review gate after milestone n.
- `DECIDE(...)` = a choice Claude Code should surface to the human, not make silently.
- "Stop and show" = print a diff summary and wait, don't auto-commit.
- All commits use Conventional Commits (`feat:`, `fix:`, `chore:`, `docs:`, `test:`, `refactor:`).

**What Claude Code should NOT do without explicit approval:**

- Change the crate layout after M0.
- Add a dependency not listed in the milestone.
- Merge to `main`.
- Delete a test (refactoring is fine; deletion needs human approval).
- Publish to crates.io.
- Skip a CHECKPOINT.

---

## 1. Reference architecture (pinned decisions)

These are fixed. Do not re-litigate in code review; push back by comment if a decision looks wrong in context, but do not silently deviate.

**Language:** Rust, edition 2021, MSRV 1.85, resolver 2.
**Async:** tokio 1.49, `rt-multi-thread`.
**MCP SDK:** rmcp 1.3+ (pin to latest stable, not a nightly).
**Transport:** stdio by default, Streamable HTTP behind `--http <addr>` flag.

**Storage:**
- SQLite (rusqlite 0.37, `bundled`) — canonical symbol/ref table, FTS5 trigram, frecency counters.
- Tantivy 0.25 — structured symbol fields only (names, qualified names, signatures, docstrings). **Not** file bodies.
- LanceDB 0.23 — vector store for optional semantic fallback.
- blake3 — content-addressable hashing.
- File watching: `notify` 8.2 + `notify-debouncer-full` 0.6, 250 ms debounce.
- Git: `gix` 0.70 (not git2).
- Walking/ignore: `ignore` 0.4 (ripgrep's).

**Parser:** `tree-sitter` 0.25 + language crates for rust, python, typescript, tsx, javascript, go, java, c, cpp. Bundle `.scm` queries from `nvim-treesitter` (MIT) and Aider (Apache-2.0) — vendor with attribution.

**Search layer (per `docs/FFF_INTEGRATION.md`):**
- `fff-grep = "=0.4.0"` — byte-level regex/exact/multi-pattern. Pinned exact, no nightlies.
- `fff-query-parser = "=0.4.0"` — optional DSL for `ext:rs git:modified size:<1mb`.
- `nucleo` 0.5 — fuzzy rescorer.
- Abstraction: `TextSearcher` trait in `ci-search` so fff can be swapped for ripgrep-subprocess.

**Embeddings (optional, feature-gated):**
- `fastembed` 5 + `ort` 2.0 — local ONNX inference.
- Model: `jina-embeddings-v2-base-code` (161M, 768-d, Apache-2.0).

**Observability:** `tracing` 0.1, `tracing-subscriber` 0.3 JSON formatter to **stderr only** (stdio transport would corrupt on stdout).

**Output shapes (discipline):** every tool returns exactly one of five shapes — `Outline`, `Skeleton`, `SymbolCard`, `ReferenceDigest`, `Diagnostics`. Defined in `ci-core::shapes`. No free-form text.

**Tool surface:** eight `code_*` tools as specified in §5 of `docs/ARCHITECTURE.md`, plus `code_find_strings` and `code_multi_find` from `docs/FFF_INTEGRATION.md`.

---

## 2. CLAUDE.md — create this file first

Create `/CLAUDE.md` with the following content verbatim. This is the agent-facing context file Claude Code reads on every session in this repo.

```markdown
# code-intel — AI agent guidance

## Before you write code
- Read `docs/IMPLEMENTATION.md` for the current milestone.
- Read `docs/ARCHITECTURE.md` if unsure about a design decision.
- Check `docs/FFF_INTEGRATION.md` before touching `crates/ci-search/`.
- Run `cargo check --workspace` to confirm the tree compiles before starting.

## Code style
- `cargo fmt` before every commit (pre-commit hook enforces).
- `cargo clippy --workspace --all-targets -- -D warnings` must pass.
- Prefer `Result<T, E>` with `thiserror`-derived errors per crate; never `anyhow` in library crates, only in `ci-cli`.
- Avoid allocation in hot paths (per-request symbol lookup, grep iteration).
- `compact_str::CompactString` for symbol names; `smallvec::SmallVec<[_; 4]>` for small fan-out collections.
- No `unwrap()` or `expect()` in library crates except in `#[cfg(test)]` or `build.rs`.
- Public API docs on every `pub` item; `#[deny(missing_docs)]` at each crate root.

## Testing
- Every new module has unit tests in a `#[cfg(test)] mod tests` block.
- Fixtures live in `crates/<crate>/tests/fixtures/`.
- Integration tests against real OSS repos go in `/tests/e2e/` (git submodules).
- Run `cargo test --workspace` before every commit.
- Bench-critical code has a `criterion` bench in `crates/<crate>/benches/`.

## Commits
- Conventional Commits: `feat(ci-parser): add rust tag extraction`.
- One logical change per commit. No `wip` commits on `main`.
- Never commit `target/`, `.code-intel/`, `*.db`, `*.tantivy/`.

## Output discipline (critical)
- MCP tool responses must be one of the five canonical shapes in `ci-core::shapes`.
- Tool descriptions must be under 2 KB each (Claude Code truncates above this).
- Log to **stderr only**. A stray `println!` anywhere in the server breaks stdio transport silently.

## Performance expectations
- p99 latency under 100 ms for all read tools on a 500K-LOC repo.
- File save → queryable in < 1 s.
- Binary size under 30 MB (release, stripped).
- Index size under 15% of repo size.

## What NOT to do
- No embedding API calls to cloud providers by default (local ONNX only).
- No `std::fs::read_to_string` on potentially large files — use mmap through `fff-grep` or `memmap2` with size caps.
- No `spawn_blocking` misuse — tantivy calls always need it; simple SQLite reads via rusqlite often don't.
- No holding `tree_sitter::Node` across `.await` points (Node: !Sync, borrows tree).
- No introducing `git2` as a dependency (we use `gix`); the only exception is if transitively pulled and unavoidable.
```

After creating CLAUDE.md, commit with message: `chore: add CLAUDE.md agent context`.

---

## 3. Repository layout (create in M0)

```
code-intel/
├── Cargo.toml                    # workspace manifest
├── rust-toolchain.toml           # pin 1.85
├── CLAUDE.md                     # §2
├── README.md                     # end-user facing; fill in M6
├── LICENSE-MIT / LICENSE-APACHE  # dual license
├── .github/workflows/            # ci.yml, release.yml
├── docs/
│   ├── ARCHITECTURE.md           # main architecture doc (imported)
│   ├── FFF_INTEGRATION.md        # search-layer addendum (imported)
│   ├── IMPLEMENTATION.md         # this file
│   └── adr/                      # architecture decision records
├── crates/
│   ├── ci-core/                  # shared types, error enums, output shapes
│   ├── ci-parser/                # tree-sitter pools, tag extraction
│   ├── ci-storage/               # rusqlite schema, migrations, blake3 hashing
│   ├── ci-search/                # Tantivy + fff-grep + TextSearcher trait
│   ├── ci-embed/                 # fastembed + lancedb (feature-gated)
│   ├── ci-indexer/               # Merkle walker, notify loop, gix diff
│   ├── ci-server/                # rmcp ServerHandler, 10 tool impls
│   └── ci-cli/                   # clap binary
├── tests/
│   └── e2e/                      # submoduled OSS repos for integration tests
├── xtask/                        # cargo xtask release, bench, schema-dump
└── benches/                      # cross-crate criterion benches
```

---

## 4. Milestone M0 — Workspace skeleton (~2 hours of agent work)

### Goal
A compiling, empty eight-crate workspace with CI configured. No functionality yet.

### Tasks
1. `cargo init --name code-intel` at repo root; convert to workspace.
2. Create `Cargo.toml` workspace manifest with `members = ["crates/*", "xtask"]`, `resolver = "2"`, and a `[workspace.dependencies]` section listing every pinned dep from §1 (do not add unlisted deps).
3. Create `rust-toolchain.toml` pinning `channel = "1.85"`.
4. Create each `crates/ci-*/` with `cargo new --lib`, set `edition = "2021"`, `rust-version = "1.85"`.
5. Create `crates/ci-cli/` as a `--bin` crate with `name = "code-intel"`.
6. Create `xtask/` as a `--bin` crate, wire it via `.cargo/config.toml` alias.
7. Create `.github/workflows/ci.yml` running: fmt check, clippy (-D warnings), test, build release.
8. Create `LICENSE-MIT` and `LICENSE-APACHE` (standard texts).
9. Create `.gitignore`: `target/`, `.code-intel/`, `*.db`, `Cargo.lock` is **committed** (binary project).
10. Copy `docs/ARCHITECTURE.md`, `docs/FFF_INTEGRATION.md`, and this `docs/IMPLEMENTATION.md` into `docs/`.
11. Create `CLAUDE.md` from §2.
12. Create a no-op lib in each crate so `cargo build --workspace` succeeds.
13. Commit: `chore: initialize workspace skeleton`.

### Acceptance (Definition of Done)
- `cargo build --workspace` passes.
- `cargo fmt --check` passes.
- `cargo clippy --workspace --all-targets -- -D warnings` passes (on an empty workspace it should).
- `cargo test --workspace` passes (zero tests, zero failures).
- `.github/workflows/ci.yml` exists and is syntactically valid (validate with `actionlint` if available).
- All files in §3 layout exist.

### CHECKPOINT(M0)
Stop. Summarize to human: "Workspace ready. 8 crates created. CI green. Ready to start M1?"

---

## 5. Milestone M1 — Core types, storage schema, error model (~1 day)

### Goal
`ci-core` and `ci-storage` compile and are unit-tested. Symbols, refs, and file metadata can be written to and read from SQLite.

### Prerequisites
M0 complete.

### Tasks

**ci-core (~3 hours):**
1. Define `Language` enum (`Rust, Python, TypeScript, Tsx, JavaScript, Go, Java, C, Cpp, Unknown`) with `from_extension(&str) -> Self` and `tree_sitter()` stub returning `Option<tree_sitter::Language>` (unimplemented until M2).
2. Define `SymbolKind` enum (`Function, Method, Struct, Class, Interface, Enum, EnumVariant, Trait, Const, Static, Type, Module, Macro, Field`).
3. Define `Symbol` struct: `id: u64, path: PathBuf, name: CompactString, qualified_name: CompactString, kind: SymbolKind, signature: Option<String>, doc: Option<String>, parent_id: Option<u64>, byte_range: Range<usize>, line_range: RangeInclusive<u32>, body_hash: [u8; 32], lang: Language`.
4. Define `Ref` struct: `src_path: PathBuf, src_symbol_id: u64, ident: CompactString, dst_symbol_id: Option<u64>, weight: f32`.
5. Define `FileMeta` struct: `path: PathBuf, lang: Language, size_bytes: u64, content_hash: [u8; 32], mtime: i64, indexed_at: i64`.
6. Define the five output shapes as an enum `ToolOutput { Outline(OutlineView), Skeleton(SkeletonView), SymbolCard(SymbolCardView), ReferenceDigest(RefDigestView), Diagnostics(DiagView) }` — concrete structs for each with `serde::Serialize`.
7. Define `ci_core::error::Error` via `thiserror` with variants for each failure class (IO, Parse, Storage, Search, Protocol).
8. Unit tests: `Language::from_extension`, roundtrip serde on each output shape, error display smoke tests.

**ci-storage (~4 hours):**
1. Add deps in crate Cargo.toml (from workspace): `rusqlite`, `rusqlite_migration`, `blake3`, `serde_json`, `thiserror`, `tracing`, `ci-core`.
2. Write `schema.sql` with the exact tables from `docs/ARCHITECTURE.md` §3: `files`, `symbols`, `refs`, `symbols_fts` (FTS5 trigram), plus `meta` table for `(key TEXT PRIMARY KEY, value TEXT)` holding schema version and last-indexed commit.
3. Implement `Store` struct wrapping a `rusqlite::Connection` (single-writer) and a pool of read connections. Use `PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;`.
4. Migration runner using `rusqlite_migration`; starts with `M001_initial.sql` containing `schema.sql`.
5. CRUD APIs: `upsert_file(&FileMeta)`, `delete_file_cascade(&Path)`, `upsert_symbol(&Symbol) -> u64`, `get_symbol_by_qname(&str) -> Option<Symbol>`, `search_symbols_fuzzy(&str, limit) -> Vec<Symbol>`, `upsert_refs(&[Ref])`, `refs_for_ident(&str) -> Vec<Ref>`.
6. Content-hash helpers: `blake3_file(&Path) -> [u8; 32]`, `blake3_bytes(&[u8]) -> [u8; 32]`.
7. Integration test: create a temp SQLite DB, insert 100 synthetic symbols, assert fuzzy search returns them, assert cascade delete removes child symbols.

### Acceptance
- `cargo test -p ci-core -p ci-storage` green.
- `cargo clippy` clean.
- Inserting 10K synthetic symbols completes in <1 s (document the number in a bench, don't write a full criterion bench yet).

### DECIDE(M1-A)
If `rusqlite_migration` is painful, fall back to a hand-rolled migration applier. **Surface this to the human only if you hit a real blocker**, not preemptively.

### CHECKPOINT(M1)
Stop. Show the human: schema DDL, list of public APIs in `ci-core` and `ci-storage`, test output. Proceed on approval.

---

## 6. Milestone M2 — Tree-sitter parser & symbol extractor (~2 days)

### Goal
Given a source file, extract all its symbols and refs. Works for Rust, Python, TypeScript, Go out of the gate. Other 5 languages wired but tag queries can be stubs.

### Prerequisites
M1 complete.

### Tasks
1. Add deps: `tree-sitter`, `tree-sitter-rust`, `tree-sitter-python`, `tree-sitter-typescript`, `tree-sitter-go`, `tree-sitter-javascript`, `tree-sitter-java`, `tree-sitter-c`, `tree-sitter-cpp`, `crossbeam-queue`, `memmap2`.
2. Vendor `.scm` query files from nvim-treesitter (MIT) into `crates/ci-parser/queries/<lang>/tags.scm` (symbols) and `locals.scm` (refs). Attribute in `crates/ci-parser/QUERIES-ATTRIBUTION.md`. If a `tags.scm` doesn't exist upstream, adapt Aider's.
3. Implement `ParserPool` per §5 of `docs/ARCHITECTURE.md`: `ArrayQueue<Parser>` per language, `with<R>(fn) -> R` pattern. One pool per `Language`.
4. Implement `extract_symbols(src: &[u8], lang: Language) -> (Vec<Symbol>, Vec<Ref>)`:
   - Run `tags.scm` query against the parsed tree.
   - For each match, classify as `def` or `ref` based on capture name.
   - For `def`: extract name, compute qualified name by walking up the AST, extract signature (first line of the symbol), extract leading docstring/comment, compute body hash.
   - For `ref`: record `(src_symbol_id, ident)`.
   - Never carry `Node<'tree>` across function boundaries — extract owned strings immediately.
5. Fixture-based tests: ship 9 small test files under `crates/ci-parser/tests/fixtures/<lang>/sample.<ext>` with an adjacent `expected.json` listing expected symbols. Run a parameterized test that parses each and compares output. **Start with Rust, Python, TS, Go having real fixtures; other 5 can have `TODO` fixtures passing trivially.**
6. Integration with `ci-storage`: a `index_file(&mut Store, path: &Path) -> Result<()>` function that reads the file (mmap if >1 MB), hashes it, skips if unchanged, otherwise parses, diffs symbols by body_hash, writes to store.

### Acceptance
- `cargo test -p ci-parser` green with all 9 languages wired (even if 5 have trivial fixtures).
- Parsing a 10 KLOC Rust file (tokio's `src/runtime/scheduler/multi_thread/worker.rs`) completes in <50 ms on a typical laptop.
- Fuzzy search in SQLite after indexing tokio's `src/` returns `Runtime` struct on query `"runtim"`.

### DECIDE(M2-A)
How to produce qualified names for Python (no namespace in AST the way Rust has)? Default: `<module_path>.<class?>.<name>`. Surface if you adopt a different convention.

### CHECKPOINT(M2)
Stop. Show human: indexing output on tokio's `src/` (symbol count by kind, timing, sample qualified names). Proceed on approval.

---

## 7. Milestone M3 — Indexer, file watcher, Merkle, git integration (~1.5 days)

### Goal
A long-running indexer process that watches a repo and keeps the index fresh within 1 second of any file save.

### Prerequisites
M2 complete.

### Tasks
1. Add deps: `notify`, `notify-debouncer-full`, `ignore`, `gix`, `rayon`, `arc-swap`.
2. Implement `MerkleTree` in `ci-indexer::merkle`: hash leaves = blake3 of file content, inner nodes = blake3 of sorted child hashes. Store snapshot as a flat `HashMap<PathBuf, [u8; 32]>` on disk via bincode or CBOR.
3. Implement `Walker`: wraps `ignore::WalkBuilder` with the default ignore set from `docs/ARCHITECTURE.md` §6 (node_modules, target, vendor, etc.), plus `.gitignore` respect, plus generated-file detection (first-8-lines marker scan).
4. Implement `ColdStart`: if `meta.last_indexed_commit` exists and matches `gix::head()`, skip re-parse entirely. Otherwise, use `gix::status()` to compute paths changed since last indexed commit, reindex only those.
5. Implement `Watcher`: `notify-debouncer-full` with 250 ms debounce. On each batch: filter through `ignore`, hash changed files, update Merkle, compute diff vs previous Merkle, enqueue affected paths into an indexing channel.
6. Implement `Indexer::run()`: reads from the channel, parses in `rayon::ThreadPool` (not tokio), upserts to Store, commits.
7. Handle `notify::Error::Overflow` / FSEvents coalescing by falling back to `git status` + targeted rehash.
8. Integration test (`tests/indexer_e2e.rs`): spin up indexer on a `tempfile::TempDir` with known files, assert indexing happens, write a new file, poll store for its symbol, assert it appears within 2 seconds.

### Acceptance
- `cargo test -p ci-indexer` green including the e2e test.
- Cold-indexing tokio's `src/` (~50K LOC) completes in <10 s on a typical laptop.
- After indexing, touching a file and waiting 1.5 s shows the updated symbols in the store.
- Re-running the indexer on an unchanged repo completes in <1 s (fast path).

### CHECKPOINT(M3)
Stop. Show human: cold-index timing on tokio and on a larger repo like the Rust stdlib or axum. Proceed on approval.

---

## 8. Milestone M4 — Search layer (fff-grep + Tantivy + TextSearcher trait) (~1.5 days)

### Goal
`ci-search` exposes: symbol exact/fuzzy (via storage), structured BM25 (via Tantivy), raw text/regex (via fff-grep). All behind trait boundaries.

### Prerequisites
M3 complete.

### Tasks
1. Add deps: `tantivy`, `fff-grep = "=0.4.0"`, `fff-query-parser = "=0.4.0"` (feature-gated `constraint-dsl`), `nucleo`, `ahash`.
2. Define `TextSearcher` trait per `docs/FFF_INTEGRATION.md` §2.
3. Implement `FffBackend`: wraps `fff_grep::Searcher`. Exposes `search(query, scope)`, `multi_search(patterns, scope)`. Handles mmap invalidation on file change via `ArcSwap<HashMap<PathBuf, Arc<Mmap>>>`.
4. **Spike first (1 hour):** before writing `FffBackend`, read `fff-grep`'s source from `~/.cargo/registry/src/...` to confirm its public API accepts an explicit path scope. If not, either (a) fork it in a patch directory, or (b) drop to direct use of `grep-matcher` + `memchr` + `aho-corasick` and treat fff's code as reference. DECIDE(M4-A) and surface.
5. Implement `TantivyBackend`: index schema with fields `id (u64 indexed)`, `name (text, CodeTokenizer)`, `qualified_name (text, CodeTokenizer)`, `signature (text, english)`, `doc (text, english)`, `lang (facet)`, `path (stored)`. Write a `CodeTokenizer` that emits CamelCase and snake_case splits alongside the original token. Only indexes symbols, never file bodies.
6. Implement `RipgrepFallback`: spawns `rg --json` as subprocess, parses line-delimited JSON. Used when `--backend rg` flag is set, or as a CI comparison target.
7. Implement `HybridSearch::query(q, mode)`: dispatches to the right backend(s), performs RRF fusion when mode=hybrid.
8. Feature-flagged DSL: `ci_search::filters::parse(&str) -> Result<Constraints>` using `fff-query-parser` when feature is on.
9. Add `nucleo`-based fuzzy rescorer in `ci-storage::search_symbols_fuzzy` (move logic or wrap). Goal: typo-resistant `find_symbol("validat_emal")` returns `validate_email`.
10. Benchmark (criterion, single bench file per backend): symbol exact lookup, symbol fuzzy, Tantivy query, fff-grep query on 500K-LOC fixture.

### Acceptance
- `cargo test -p ci-search --all-features` green.
- `code_search "retry_policy" filter:"lang:rust"` (programmatic, not MCP yet) on a tokio-sized repo returns hits in <20 ms.
- Fuzzy symbol search for a known typo returns the right symbol in the top 3.
- Benchmarks run without panic and produce numbers (don't assert specific thresholds yet).

### DECIDE(M4-B)
If fff-grep's API is too limited for an explicit-scope query, choose between fork (higher cost, upstream later) or drop-down to `grep-matcher` (loses the SIMD-prefiltered multi-grep path). Document the decision in `docs/adr/001-text-search-backend.md`.

### CHECKPOINT(M4)
Stop. Show human: benchmark output comparing fff-grep vs ripgrep on the same query over the same corpus. Proceed on approval.

---

## 9. Milestone M5 — MCP server + the ten tools (~2.5 days)

### Goal
A working stdio MCP server that Claude Code can connect to and use. All ten tools implemented, described under 2 KB each, returning one of the five canonical shapes.

### Prerequisites
M4 complete.

### Tasks
1. Add deps: `rmcp` (pin latest stable), `tokio` full features, `serde`, `serde_json`, `schemars` if rmcp requires it.
2. Implement `CodeIntelServer` in `ci-server` deriving rmcp's `ServerHandler`. Holds `Arc<Store>`, `Arc<HybridSearch>`, `Arc<Indexer>` (read side).
3. Implement the ten tools as `#[tool]` methods (or whatever rmcp's macro is — confirm from its docs before coding):
   - `code_outline(path)`
   - `code_skeleton(path, depth=2)`
   - `code_read_symbol(path, symbol_or_line)`
   - `code_find_symbol(name, kind?, lang?)`
   - `code_find_refs(symbol)`
   - `code_search(query, mode, filter?)`
   - `code_list(glob?, lang?, filter?)`
   - `code_repo_map(focus_files?, token_budget=2000)`
   - `code_find_strings(pattern, kind="literal"|"comment"|"both")`
   - `code_multi_find(patterns[])`
4. For each tool, the description string (under 2 KB) must include: one-sentence purpose, explicit "prefer over Read/Grep/Glob when…", 1-2 input examples, output shape name, typical token size. Store descriptions as module constants so they're greppable.
5. Implement the `code_repo_map` PageRank renderer:
   - Build `petgraph::Graph<Symbol, f32>` from `refs` table.
   - Apply edge-weight heuristics from `docs/ARCHITECTURE.md` §2 (Aider's): 10× identifiers in focus files, 0.1× `_private`, 0.1× >5-file-common, 50× refs from focus files.
   - Run personalized PageRank via power iteration (50 iterations, damping 0.85).
   - Binary-search pack top symbols into a skeleton view under `token_budget` (use `tiktoken-rs` for counting; aim within 15% of budget).
6. Wire `clap` in `ci-cli`: `code-intel serve [--http ADDR] [--repo PATH] [--log LEVEL]`, `code-intel index [--repo PATH]`, `code-intel query <tool> <args...>` for debugging without an MCP client.
7. **stdout hygiene test**: a test that runs `code-intel serve` in a subprocess, pipes an MCP `initialize` request, captures stdout, and asserts every line is valid JSON-RPC.
8. MCP integration test: use `rmcp`'s test utilities (or a manual mock client) to send each of the ten tool calls and assert response shape.
9. Logging: `tracing_subscriber::fmt().with_writer(std::io::stderr).json().init()`.
10. Path traversal guard: every `path` argument goes through `canonicalize_within_root(&root, &path)` helper that rejects escape.

### Acceptance
- `cargo test -p ci-server` green including stdout-hygiene test.
- `code-intel serve` starts, accepts an `initialize` request, lists ten tools, responds to a `code_outline` call with a valid Outline shape.
- Every tool description is under 2 KB (enforced by a `const_assert!` or test).
- Manual Claude Code connection test: in a sample repo, install the server, issue "outline the auth module" — Claude picks `code_outline`, gets back a skeleton, returns a useful answer. DECIDE(M5-A) if Claude picks Read instead — tune descriptions and the recommended `.claude/settings.json` disable-hook.

### CHECKPOINT(M5)
Stop. Show human: transcript of a real Claude Code session using the server on a real repo. Measure input tokens vs a baseline Claude Code session on the same task. Proceed on approval with the measured numbers in hand.

---

## 10. Milestone M6 — Polish, testing, release (~1.5 days)

### Goal
Ship 0.1.0 on crates.io and as a prebuilt binary. Installation one-liner works. Observability + multi-repo + degraded-mode work.

### Prerequisites
M5 complete.

### Tasks
1. **End-to-end test suite**: add three real repos as git submodules under `tests/e2e/repos/`: `aider`, `ripgrep`, `axum`. Write a harness that indexes each and runs a fixed set of tool calls against known ground truth in YAML.
2. **SWE-Bench Lite retrieval eval**: for 10 instances, index the repo, use `code_find_symbol` + `code_repo_map` to try to surface the gold files; measure hit rate. Target: ≥60% on the sample, will tune later.
3. **Secret redaction**: implement a regex-set scanner (`aws_access_key`, `openai_key_pattern`, `pem_private_key_header`, etc.) applied to every tool response that includes file bytes; replace matches with `***REDACTED***`. Block paths matching `.env*, *.pem, *.key, id_rsa*` unless `allow_sensitive: true`.
4. **Multi-repo support**: `DashMap<RepoRoot, RepoState>`, `code_activate_repo(path)` tool, per-repo SQLite/Tantivy dirs under `~/.cache/code-intel/<hash_of_root>/`.
5. **Metrics endpoint**: when `--http` is set, expose `/metrics` Prometheus via `axum` (lightweight — a few counters and histograms, not full OTLP yet).
6. **Release tooling**: `cargo-dist` config for prebuilt macOS (Intel+ARM), Linux (x86_64+ARM64), Windows (x86_64) binaries. Signing skipped for 0.1, TODO for 0.2.
7. **Install script**: `scripts/install.sh` that detects platform, downloads prebuilt binary, prints `.claude/settings.json` snippet to wire up the MCP server + disable Read/Grep/Glob.
8. **README.md**: quickstart (`curl | bash`), feature list, token-economics section with measured numbers from M5, comparison table with Serena/fff-search/codebase-memory-mcp, licensing.
9. **ADRs written**: `docs/adr/000-symbol-first-architecture.md`, `001-text-search-backend.md` (from M4), `002-output-shape-discipline.md`, `003-stdio-transport-hygiene.md`.
10. **Tag v0.1.0**, push, let `cargo-dist` publish binaries to GitHub Releases.

### Acceptance
- All e2e tests green.
- SWE-Bench Lite retrieval hit rate documented in README.
- `curl -L <install-url> | bash` installs the server on a clean macOS and a clean Linux VM, and it works with Claude Code out of the box.
- Binary size under 30 MB stripped.
- Release is live on GitHub Releases.

### CHECKPOINT(M6)
Stop. v0.1.0 is live. Write a launch post draft (the human will edit and publish).

---

## 11. Test strategy summary

| Layer | Tool | Where |
|---|---|---|
| Unit | `cargo test` per crate | `#[cfg(test)] mod tests` in each file |
| Fixture | Parameterized parser tests | `crates/ci-parser/tests/fixtures/` |
| Property | `proptest` on Merkle diff | `crates/ci-indexer/tests/prop_merkle.rs` |
| Integration | Indexer e2e, storage e2e | `crates/*/tests/` |
| E2E | Real OSS repos as submodules | `tests/e2e/` |
| Protocol | MCP mock client | `crates/ci-server/tests/mcp_client.rs` |
| Performance | Criterion benches | `crates/*/benches/` |
| Regression | Nightly SWE-Bench Lite retrieval | `.github/workflows/swe_bench.yml` (cron) |
| Release gate | Token-econ benchmark | `tests/token_econ/` — replay a canned session |

## 12. Checkpoint summary (the only places to stop)

- `CHECKPOINT(M0)` — after workspace skeleton, before M1.
- `CHECKPOINT(M1)` — after core + storage, before M2.
- `CHECKPOINT(M2)` — after parser, before M3.
- `CHECKPOINT(M3)` — after indexer, before M4.
- `CHECKPOINT(M4)` — after search layer, before M5.
- `CHECKPOINT(M5)` — after MCP server, before M6.
- `CHECKPOINT(M6)` — release complete.

Between checkpoints, Claude Code runs autonomously. At each checkpoint, the human reviews and approves before Claude Code proceeds.

---

## 13. Time and cost estimates — with and without Claude Code

**Baseline engineering effort** (solo developer, experienced Rust, no AI assistance):

| Milestone | Human-only estimate |
|---|---|
| M0 Workspace | 0.5 day |
| M1 Core + Storage | 2 days |
| M2 Parser | 3 days |
| M3 Indexer | 2 days |
| M4 Search | 2 days |
| M5 MCP Server | 3 days |
| M6 Polish/Release | 2 days |
| **Total calendar days** | **14–15 focused days (~3 weeks of real time at 5 hrs/day)** |

Focused days assume an experienced Rust developer working on this full-time with minimal context switching. Real-time calendar duration is typically 2× that if working part-time.

**With Claude Code, realistic compression per milestone:**

The compression factor is not uniform. Tasks break into three categories:

| Task type | Speedup with Claude Code | Why |
|---|---|---|
| Boilerplate (workspace, Cargo.toml, schemas, derives, trait impls, error enums, CLI args, tests scaffolding) | **3–5×** | Claude writes these fluently; human just reviews |
| Algorithmic / novel logic (PageRank renderer, Merkle diff, custom tokenizer, fuzzy scoring, query planner) | **1.5–2×** | Claude helps but human architects; iteration on real data required |
| Integration debugging (tree-sitter query edge cases, fff-grep API surface, MCP stdio corruption, Claude Code prompts picking Read over code_*, notify edge cases on macOS) | **1.0–1.3×** | The hard part is running the thing and observing; Claude can suggest fixes but can't iterate the system itself |
| Performance tuning (profiling, allocation analysis, mmap gotchas, tantivy commit cadence) | **1.0–1.5×** | Bottlenecked on measurement, not code |

**Per-milestone realistic estimates with Claude Code (Max 5x or 20x):**

| Milestone | Human-only | With Claude Code | Compression |
|---|---|---|---|
| M0 Workspace | 0.5 d | **1–2 hr** | ~3× |
| M1 Core + Storage | 2 d | **0.5–1 d** | ~3× |
| M2 Parser | 3 d | **1–1.5 d** | ~2× (tag-query debugging is slow) |
| M3 Indexer | 2 d | **1 d** | ~2× |
| M4 Search | 2 d | **1–1.5 d** | ~1.5× (fff-grep API spike) |
| M5 MCP Server | 3 d | **1.5–2 d** | ~1.7× (stdio hygiene debugging) |
| M6 Polish/Release | 2 d | **1 d** | ~2× |
| **Total focused days** | **14–15** | **6–9** | **~2× overall** |

**Calendar time** (assuming part-time, 5-hour sessions):

| Plan | Limit | Feasible pace | Calendar time to v0.1 |
|---|---|---|---|
| No AI | — | 1 focused day/day | **3 weeks** |
| Pro ($20) | ~45 msg/5h | 1 milestone/session constrained; frequent waits | **2.5–3 weeks** (limit waits eat the speedup) |
| Max 5x ($100) | ~225 msg/5h | 1–2 milestones/session comfortably | **10–14 calendar days** |
| Max 20x ($200) | ~900 msg/5h | 2–3 milestones/session, no limit anxiety | **7–10 calendar days** |

**What Max buys you that Pro doesn't:**
- **Long autonomous sessions**: Max 5x handles an entire milestone (M1, M2, M4, M6) end-to-end in one 5-hour window without hitting limits. Pro typically forces a mid-milestone wait.
- **Context window headroom**: M5 in particular requires reading rmcp's docs, your own ARCHITECTURE.md, FFF_INTEGRATION.md, and existing crate code in one session — that's ~50K tokens of context before any work happens. Pro users often `/clear` and re-load context; Max users don't.
- **Weekly limit comfort**: The project is ~6–9 focused days of Claude Code work. On Pro's weekly limit, that's tight even spread over two weeks. Max 5x clears it comfortably; Max 20x is overkill for one developer on one project.
- **Priority queue**: Claude Code sessions stay responsive during peak hours; Pro users can see 5–15 s response latency at peak, which breaks flow on rapid iteration tasks.

**Max 20x vs Max 5x for this project**: If you're a solo developer building this single project, **Max 5x is the right tier**. Max 20x only pays off if you're running multiple concurrent projects or leaving agents unsupervised for long autonomous coding runs (not recommended on this codebase — the CHECKPOINT gates are load-bearing).

**Honest caveats:**

- The 2× headline number assumes you review Claude Code's output. If you rubber-stamp, you pay the cost later in debugging; effective compression drops to ~1.3–1.5×.
- M2 (parser) and M4 (search) are the least compressible milestones because they require iterative testing against real source files — Claude can write code fast but needs you or itself to run it, observe, and adjust. Budget conservatively here.
- M5 (MCP server) has a hidden tax: Claude Code tends to over-describe its own tools (the descriptions go over 2 KB and silently truncate). Enforce the length limit in a test and reject overlong descriptions aggressively.
- Don't try to do M0–M6 in one marathon. The CHECKPOINT gates are real — if you skip human review after M2 (parser), a wrong qualified-name convention propagates into M3/M4/M5 and costs more to unwind than the review saved.
- When debugging a production issue later, Claude Code's speedup collapses to ~1.2× because the bottleneck is reproduction and measurement, not code writing. Don't budget for permanent 2× — it's an MVP acceleration, not a steady-state multiplier.

**Recommended plan:** Max 5x for the MVP build, then re-evaluate. If you're past M6 and actively iterating on the production milestones (SCIP, LSP, multi-repo), Max 5x remains sufficient. Move to Max 20x only if you're concurrently working on something else that also uses Claude heavily.

---

## 14. How to kick off (first prompt to Claude Code)

After creating this file in `docs/IMPLEMENTATION.md`, from an empty repo directory:

```
claude "Read docs/IMPLEMENTATION.md end to end. Then start Milestone M0.
Do not proceed past CHECKPOINT(M0) without showing me the output.
Use your TODO tool to track tasks. Commit after each logical unit of work."
```

After M0, M1, … approve with:

```
claude "M{n} approved. Start M{n+1} per docs/IMPLEMENTATION.md.
Same rules: stop at CHECKPOINT, commit per task, run tests."
```

If Claude Code gets stuck or confused, reset context and re-anchor:

```
claude "/clear"
claude "Re-read CLAUDE.md and docs/IMPLEMENTATION.md §{section}.
Current state: {paste last git log}. Resume from {specific task}."
```

---

## 15. Out-of-scope for v0.1 (explicit deferrals)

These are planned for v1/production per the main architecture doc and must **not** be started in v0.1:

- SCIP consumption (tier-5 precision)
- Live LSP delegation
- Wasm-runtime grammar loading for niche languages
- Embedding model (feature flag scaffolded in M6, but disabled by default — flip on in v1)
- OTLP tracing (Prometheus metrics only for now)
- Team-shared indexes (Cursor-style simhash reuse)
- Code-editing tools (this server is read-only)
- Windows as a primary target (builds, but CI green is the bar, not platform parity)

Attempting these during v0.1 will extend the timeline by 2–3× and is a CHECKPOINT violation. Flag and defer.
