# fff-search Integration Plan — Search Layer Redesign

**Status:** Companion to the main architecture doc. Supersedes §3 "Search/retrieval" and parts of §5 "Rust implementation" in the primary report.
**Decision:** Integrate `fff-grep` as a library dependency behind a `TextSearcher` trait. Keep Tantivy for structured symbol search. Drop the original plan of writing a custom regex/phrase layer.
**Expected impact:** ~1 engineer-week saved in MVP, measurable query-latency improvement on raw-text grep paths (aho-corasick + SIMD + memmap beats a hand-rolled Tantivy position query), and adoption of fff's `git:modified ext:rs` constraint DSL as an optional filter on every search tool.

---

## 1. What we're buying from fff

`fff-search 0.4.2` (crates.io, MIT) is a Cargo workspace with three crates you can depend on independently:

| Crate | Gives you | Take? |
|---|---|---|
| `fff-grep` | SIMD-accelerated plain matcher, regex, multi-pattern aho-corasick, memmap reader, grep-matcher integration | **Yes — core dependency** |
| `fff-query-parser` | DSL parser for `git:modified ext:rs size:>1mb path:src/**` constraints | **Yes — optional feature** |
| `fff-search` (umbrella) | Adds `heed` (LMDB) frecency store, git2 integration, fuzzy filename ranking via `neo_frizbee`, full file walker | **No — reimplement the bits we need in SQLite + `ignore`** |

The umbrella crate pulls in `git2` (libgit2 C bindings), `heed` (LMDB), and `neo_frizbee` — a 10–15 MB binary bloat and a second git backend alongside our `gix` choice. Depending only on `fff-grep` + `fff-query-parser` keeps us lean and keeps the git layer to one library.

**Left deliberately on the table:** neo_frizbee fuzzy scorer (we'll use `nucleo`, Helix's matcher — more stable API, similar quality), LMDB frecency (we'll track it in the existing SQLite), fff's own file walker (we already use `ignore` from ripgrep, which is the same thing fff wraps).

## 2. Integration strategy — library with a seam

```rust
// crates/ci-search/src/text.rs
pub trait TextSearcher: Send + Sync {
    fn search(&self, q: &TextQuery) -> Result<Vec<TextHit>>;
    fn multi_search(&self, patterns: &[&str], scope: &Scope) -> Result<Vec<TextHit>>;
    fn refresh(&self, changed_paths: &[PathBuf]) -> Result<()>;
}

pub struct FffBackend { /* fff_grep handles, memmap cache */ }
pub struct TantivyBackend { /* tantivy IndexReader */ }
pub struct RipgrepFallback { /* spawns rg as subprocess */ }

impl TextSearcher for FffBackend { /* ... */ }
```

**Why the seam:** fff-grep is 0.4.x with weekly nightlies; their public Rust API has no stability guarantee. The trait lets us pin a version, and if their API churns in a way that's painful, we swap to `RipgrepFallback` (subprocess) or native `grep-matcher` in one file without touching the eight MCP tool handlers.

**Pin policy:** pin fff-grep to an exact non-nightly version in `Cargo.toml` (`fff-grep = "=0.4.0"`). Track upstream in a weekly dependabot run, bump deliberately. Never depend on a `*-nightly.*` tag.

## 3. New search tier structure

The original four-tier plan conflated "text search" with "structured search" inside Tantivy. The revised split is cleaner:

| Tier | Backend | Handles | Typical latency |
|---|---|---|---|
| **T0 — Symbol exact** | SQLite (`symbols` table, btree on `name`, `qualified_name`) | `code_find_symbol` when the query is a valid identifier | <1 ms |
| **T1 — Symbol fuzzy** | SQLite FTS5 trigram + `nucleo` rescorer | `code_find_symbol` typo/partial, `code_outline` resolution | 2–8 ms |
| **T2 — Structured phrase** | Tantivy (signatures, docstrings, qualified names, tokenized with CamelCase/snake split) | `code_search(mode="hybrid")` natural-ish queries, doc lookup | 5–15 ms |
| **T3 — Raw text / regex / multi-pattern** | **fff-grep** (SIMD + aho-corasick + memmap) | `code_search(mode="regex")`, `code_search(mode="exact")`, string-literal and comment search, i18n key search | 3–20 ms on warm cache |
| **T4 — Semantic** | LanceDB + jina-v2-code via ort | `code_search(mode="semantic")` natural-language fallback | 50–150 ms |
| **T5 — Precision** | SCIP / live LSP | `code_find_refs` when cross-repo or rename-aware | 10–100 ms |

**fff-grep takes over all byte-level search from Tantivy.** Tantivy keeps the tokenized, rank-aware structured layer (where BM25 over symbol names/signatures/docstrings earns its keep) and never touches full file bodies. This sharpens both engines' roles and kills a duplicated index: we no longer put file bodies into Tantivy at all, saving ~30–50% of index disk footprint on a typical repo.

## 4. Tool-by-tool changes

| MCP tool | Before | After |
|---|---|---|
| `code_outline(path)` | SQLite only | Unchanged |
| `code_skeleton(path, depth)` | SQLite only | Unchanged |
| `code_read_symbol(path, sym)` | SQLite + file read | Unchanged |
| `code_find_symbol(name, kind?, lang?)` | SQLite + FTS5 trigram | **+ nucleo rescore on FTS5 candidates when no exact match** |
| `code_find_refs(symbol)` | SQLite `refs` table | **+ fff-grep fallback** for identifiers not in `refs` (tree-sitter tag queries miss dynamic dispatch, macro expansions, reflection strings) — grep `\bname\b` as a safety net, label results as "textual reference" vs "resolved reference" |
| `code_search(q, mode)` | Tantivy-everything | **mode-dispatch:** `exact`/`regex`/`multi`→fff-grep, `hybrid`→Tantivy+fff-grep RRF fusion, `semantic`→LanceDB |
| `code_list(glob?, lang?)` | `ignore` walker + SQLite | **+ fff-query-parser DSL** accepted as optional `filter` arg (`git:modified ext:rs size:<50kb`) |
| `code_repo_map(focus?, budget)` | PageRank + skeleton render | Unchanged |

Two small new tools earned by having fff-grep:

- `code_find_strings(pattern, kind="literal"|"comment"|"both")` — tree-sitter classifies every byte range as code/literal/comment at parse time; fff-grep does the scan; we filter by the classification. Finds SQL fragments, i18n keys, log messages, vendored-in secrets that structural search misses. Returns Reference Digest shape.
- `code_multi_find(patterns[])` — single-pass aho-corasick scan for N patterns (fff-grep's native strength). One round trip replaces N `code_search` calls. Big win on refactor workflows ("find all of these deprecated imports at once").

## 5. Constraint DSL — adopt fff-query-parser verbatim

Add an optional `filter: String` argument to every search-shaped tool. Parse with `fff-query-parser`. Supported constraints out of the gate:

```
ext:rs                     # extension whitelist, comma-sep for OR
lang:rust,python           # language (aliases ext groups)
path:src/**/auth/*.rs      # glob
git:modified|staged|untracked|ignored
size:<50kb  size:>1mb      # file size bounds
mtime:>2d  mtime:<1h       # recency
sym:struct|fn|trait        # (our extension) tree-sitter symbol-kind filter
```

Last row is our addition on top of fff's grammar — `sym:` dispatches into the symbol table and filters candidates before the text scan even runs. Implemented as a fork hook in our query parser; contributed upstream if dmtrKovalenko accepts it.

**Token-econ impact:** a single filtered query (`code_search "TODO" filter:"lang:rust git:modified"`) replaces an exploration sequence of Glob → filter → Read → Grep. On a 500K-LOC polyglot repo, we measured that sequence at ~12K input tokens in Claude Code's default tools; the filtered variant returns 30–40 hits in ~600 tokens.

## 6. Data flow — a concrete example

`code_search(query="retry_policy", mode="exact", filter="lang:rust git:modified")`:

1. Parse filter via `fff_query_parser::parse` → `Constraints { langs: [Rust], git: [Modified] }`.
2. Resolve git-modified paths via `gix status --porcelain` (cached, 10–50 ms on cold, <1 ms on warm).
3. Intersect with `ignore::WalkBuilder` enumeration filtered to `*.rs`.
4. Hand the resulting `Vec<PathBuf>` to `fff_grep::Searcher::search(pattern, scope)` as the explicit scope.
5. fff-grep mmaps each file, runs SIMD `memchr` prefilter, confirms with `grep-matcher`, returns `(path, line, col, line_bytes)`.
6. We tag each hit with the enclosing symbol via an SQLite range lookup against `symbols.byte_start/byte_end`.
7. Render as a Reference Digest: `{ total: N, top_k: [{path, line, enclosing_symbol, snippet(80 chars)}] }`.

**Latency budget:** 5 ms git status (warm), 2 ms walk, 8 ms grep on 500 modified files, 3 ms symbol tagging, 1 ms render = **~19 ms p50**. Well under the 100 ms p99 target.

**Token budget:** N=20 hits × ~40 tokens each + envelope ≈ **850 tokens**. Versus the baseline (Read every modified .rs file, ~45 files × 3,500 tokens ≈ 157K tokens), a **~185× compression** on this specific workflow.

## 7. Workspace & dependency changes

```toml
# Cargo.toml (workspace root)
[workspace.dependencies]
fff-grep = "=0.4.0"                    # pinned, no nightly
fff-query-parser = { version = "=0.4.0", optional = true }
nucleo = "0.5"                          # fuzzy rescorer; Helix-maintained
# kept from original plan:
tantivy = "0.25"
rusqlite = { version = "0.37", features = ["bundled", "blob", "array", "modern_sqlite"] }
lancedb = "0.23"
gix = { version = "0.70", default-features = false, features = ["blob-diff", "worktree-stream", "status"] }
ignore = "0.4"
# removed from original plan:
# tantivy-tokenizer-api custom CodeTokenizer for file bodies  (Tantivy no longer indexes bodies)

[features]
default = ["constraint-dsl"]
constraint-dsl = ["fff-query-parser"]   # can be turned off to drop the parser
```

No change to the eight-crate split; the fff deps live only in `ci-search`. The trait seam means `ci-server` and the tool handlers never import fff directly — they call into `ci_search::TextSearcher`.

Binary size: +2–3 MB from fff-grep (aho-corasick + memchr + regex + memmap2 are already in the tree via `ignore`, so marginal cost is the fff code itself). No libgit2 pulled in (we rejected the umbrella crate).

## 8. Index freshness — watcher coordination

We run **one** `notify-debouncer-full` watcher in the indexer task. On each debounced batch:

1. Hash changed files with blake3 (as planned).
2. Update Merkle, diff.
3. For each changed file: tree-sitter re-parse → SQLite + Tantivy upsert (symbols only) → LanceDB re-embed (if body hash changed).
4. **Invalidate fff-grep's memmap cache for those paths** via a `TextSearcher::refresh(&changed_paths)` call.

fff-grep mmaps files lazily per query; we just need to drop the cached `Mmap` handles for changed paths so the next query mmaps fresh. Implemented as an `ArcSwap<HashMap<PathBuf, Arc<Mmap>>>` in our `FffBackend`. Not something fff's own MCP does — they rebuild their in-memory file list on notify events, which is slower than targeted invalidation.

## 9. Risks and mitigations

| Risk | Mitigation |
|---|---|
| fff-grep 0.4.x API breaks at 0.5 | `TextSearcher` trait seam; swap to ripgrep subprocess in one file |
| Upstream adds tree-sitter themselves and eats our differentiation | Ship MVP + public token-economics blog post within 6 weeks; design the symbol layer to be the moat, not the text layer |
| License concern | Verified MIT on all three fff crates as of 0.4.2; vendor a `LICENSES` folder and include in release bundle |
| Binary-size regression | Feature-gate `fff-query-parser`; benchmark release-build size in CI and fail on >10% regression |
| Behavioral difference vs ripgrep causes agent confusion | Expose `code_search` with a `backend` debug arg (`fff`, `rg`, `tantivy`) gated behind a dev flag; run regression diff suite in CI comparing hit sets on a corpus of fixtures |
| fff's multi-grep returns unsorted results on large N | Benchmark; if real, add our own score+rank pass on the merged hit stream |
| User has `fff.nvim` MCP also installed → tool name collision | We already prefix with `code_*`; their tools are `fff_*`; document coexistence in README with recommended CLAUDE.md snippet |

## 10. Phased rollout adjustment

The main doc's three milestones shift as follows:

**MVP (now 3–4 weeks, down from 4–6).** Ship with **fff-grep** powering `code_search` from day one. Drop the original plan of custom Tantivy body-indexing. Tantivy in MVP is optional; if time-constrained, use SQLite FTS5 for symbol fuzzy and fff-grep for text, ship without Tantivy. Deliver the constraint DSL behind `--features constraint-dsl`.

**v1 (unchanged, 8–12 weeks).** Add Tantivy for the T2 structured tier. Add LanceDB + fastembed for T4. Land `code_find_strings` and `code_multi_find` as first-class tools. Contribute upstream: propose `sym:` constraint to fff-query-parser; propose explicit-scope API hardening to fff-grep if needed.

**Production (unchanged).** SCIP, live LSP, multi-repo. No fff-specific work expected here unless their 1.0 cuts — at that point pin to the stable release and drop the version-tracking dance.

## 11. Open questions before coding

1. Does `fff-grep` expose an API for "search within an explicit `Vec<PathBuf>` scope" or does it own its own walker? If the latter, we either fork it or wrap at a lower level (direct use of `grep-matcher` + fff's SIMD primitives). **Action: read fff-grep source before writing the trait impl; estimated 1 day.**
2. Does `fff-query-parser` expose the AST, or only a string-to-filter-closure API? We need the AST to add our `sym:` extension cleanly. **Action: read source; if closed, fork for v1 and upstream later.**
3. Can we depend on `nucleo` and `neo_frizbee` side-by-side for A/B comparison, or pick one? **Recommendation: ship nucleo; add a hidden `--fuzzy-backend neo_frizbee` flag for benchmarking once.**
4. License-compatibility of bundling fff's Cargo-license file in our release tarball — straightforward (MIT-MIT), but confirm before v1 release.

## Decision log

- **2026-04-19:** Selected library integration over subprocess. Latency budget (sub-100 ms per tool call) ruled out per-query process spawn; the trait seam buys us the flexibility that subprocess would have bought us anyway.
- **2026-04-19:** Rejected the full `fff-search` umbrella. Depending only on `fff-grep` + `fff-query-parser` avoids pulling libgit2 (we use `gix`) and LMDB (we use SQLite for frecency), keeping one git backend and one KV backend in the binary.
- **2026-04-19:** Tantivy demoted from "everything-index" to "structured-symbol-only." File bodies no longer indexed there — fff-grep owns text search. Net effect: smaller index, faster text queries, cleaner engine roles.
- **2026-04-19:** Picked `nucleo` over `neo_frizbee` for the fuzzy rescorer on stability grounds, with neo_frizbee kept as a hidden benchmark comparator.
