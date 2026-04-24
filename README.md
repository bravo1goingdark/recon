<p align="center">
  <img src="site/og.png" alt="recon — 35x fewer tokens for AI coding agents" width="800" />
</p>

<h1 align="center">recon</h1>

<p align="center">
  Token-lean code intelligence MCP server.<br/>
  Replaces <code>Read</code>, <code>Grep</code>, and <code>Glob</code> with symbol-aware tools that deliver <strong>15-30x token reduction</strong> on code exploration.
</p>

<p align="center">
  <a href="https://mcprecon.pages.dev">Website</a> · <a href="https://mcprecon.pages.dev/Docs.html">Docs</a> · <a href="#connect-your-agent-hosted">Get Started</a>
</p>

---

Offered as a **hosted MCP server** -- no binary to install, no local indexing. Point your agent at the endpoint, hand it an API key, and it gets 12 symbol-aware tools instantly.

## Benchmarks

Measured on real codebases, release build, warm cache:

| | Zed (80K symbols) | Rust compiler (320K symbols) |
|---|---|---|
| **stats** | 10 ms | 29 ms |
| **find** | 8 ms | 8 ms |
| **search** | 11 ms | 33 ms |
| **outline** | 14 ms | 13 ms |
| **skeleton** | 11 ms | 11 ms |
| **refs** | 8 ms | 12 ms |
| **map (cached)** | 8 ms | 19 ms |
| **map (cold)** | 405 ms | 2.0 s |
| **cold index** | 19 s | 53 s |

All read-path queries under 33 ms on 320K symbols. Binary size: 24 MB.

## Token reduction

Measured on the Rust compiler (318K symbols), recon vs Read/Grep/Glob:

| Scenario | Before | After | Reduction |
|---|---|---|---|
| Read one function | ~23,838 tok | ~111 tok | **215x** |
| Find a symbol | ~17,500 tok | ~226 tok | **77x** |
| Repo orientation | ~52,500 tok | ~2,170 tok | **24x** |
| Find references | ~15,000 tok | ~638 tok | **24x** |
| Outline a file | ~23,838 tok | ~1,350 tok | **18x** |
| Understand a file | ~23,838 tok | ~6,412 tok | **3.7x** |

A typical "find and fix a bug" task: **~3.2K tokens** with recon vs **~100K+** with Read/Grep/Glob.

## Connect your agent (hosted)

### Step 1: Get an API key

Contact us to get a server key for your workspace.

### Step 2: Add to your MCP config

Drop this into `.mcp.json` at your project root:

```json
{
  "mcpServers": {
    "recon": {
      "url": "https://mcp.recon.dev/v1",
      "headers": {
        "Authorization": "Bearer YOUR_API_KEY"
      }
    }
  }
}
```

### Step 3: Teach your agent

Add this to your `CLAUDE.md` (or equivalent agent system prompt):

```markdown
Prefer code_* tools (code_outline, code_skeleton, code_find_symbol,
code_search, code_repo_map) over Read/Grep/Glob for code exploration.
They return structured, token-efficient results.
```

### Step 4: Restart your agent

Restart Claude Code (or your MCP client). The 12 `code_*` tools are now available. Indexing, watching, and ranking all run server-side.

## Setup

```bash
# 1. Authenticate once per machine (caches license globally)
recon login sk-recon-your-key

# 2. In each project: index + set up IDE MCP config
recon init --mcp cc        # Claude Code  (.mcp.json)
recon init --mcp oc        # OpenCode     (.opencode/mcp.json)
recon init --mcp cursor    # Cursor       (.cursor/mcp.json)
recon init --mcp windsurf  # Windsurf     (.windsurf/mcp.json)
recon init                 # Index only, no MCP config

# Your IDE auto-starts recon serve — you never run it manually.
```

Other license commands:

```bash
recon license    # show cached tier, limits, expiry
recon logout     # remove cached license
```

## Tools (12)

| Tool | Replaces | What it does | Latency |
|---|---|---|---|
| `code_outline(path)` | Read | One line per symbol -- kind, name, line | 13 ms |
| `code_skeleton(path)` | Read | Signatures + docs, bodies as `...` (10x compression) | 11 ms |
| `code_read_symbol(path, symbol)` | Read | Full source of one symbol + callers | <10 ms |
| `code_find_symbol(name)` | Grep | 3-tier: exact SQLite -> Tantivy BM25 -> FTS5 + nucleo fuzzy | 8 ms |
| `code_find_refs(symbol)` | Grep | Reference count + top-k call sites | 12 ms |
| `code_search(query, mode, filter?)` | Grep | exact/regex/hybrid + filter DSL, Tantivy-first | 33 ms |
| `code_list(glob?, lang?, filter?)` | Glob | Structured file listing with symbol counts (batch query) | 57 ms |
| `code_repo_map(budget)` | -- | PageRank-ranked symbol overview, cached in SQLite | 19 ms |
| `code_find_strings(pattern)` | -- | Search string literals and comments | <30 ms |
| `code_multi_find(patterns[])` | -- | Multi-pattern search in one call | <30 ms |
| `code_reindex()` | -- | Agent-triggered re-indexing, clears map cache | varies |
| `code_stats()` | -- | Index health: files, symbols, freshness | 10 ms |

### Filter DSL

Search tools accept an optional `filter` parameter:

```
*.rs                   # extension filter
type:rust              # language type
status:modified        # git-modified files only
!test                  # exclude paths containing "test"
/src/                  # path segment match
```

## Architecture

```
crates/
  recon-core/       # Types, errors, 5 output shapes, config, secret redaction
  recon-parser/     # Tree-sitter pools (9 langs), symbol extraction
  recon-storage/    # SQLite + FTS5 trigram, blake3, batch inserts
  recon-search/     # Tantivy BM25, fff-grep, nucleo fuzzy, PageRank, token counting
  recon-embed/      # fastembed + LanceDB vector search (feature-gated)
  recon-indexer/    # Merkle tree, gix ColdStart, file watcher, rayon parallel parse
  recon-server/     # rmcp MCP handler, 12 tools, parking_lot Mutex, redaction
  recon-cli/        # CLI: login, init, serve, index, purge, query tools
```

### Search tiers

| Tier | Backend | Latency |
|---|---|---|
| T0 -- Symbol exact | SQLite btree index | <1 ms |
| T1 -- Symbol fuzzy | SQLite FTS5 trigram + nucleo rescore | 2-8 ms |
| T2 -- Structured BM25 | Tantivy with CodeSplitTokenizer | 5-15 ms |
| T3 -- Raw text/regex | fff-grep (SIMD + memmap2) | 3-95 ms |
| T4 -- Semantic | fastembed + LanceDB (feature-gated) | 50-150 ms |

### Incremental indexing

1. **ColdStart** -- gix reads HEAD SHA. If unchanged since last index, skip entirely.
2. **Merkle diff** -- blake3 hash tree. On HEAD change, reindex only changed files.
3. **Full index** -- first run, parallel parse via rayon.
4. **Live watcher** -- notify-debouncer-full (250 ms debounce) triggers per-file reindex.

### PageRank repo map

`code_repo_map` builds a directed graph from symbol references, applies Aider-style edge weights (10x long identifiers, 0.1x private names, 50x focus files), runs power iteration with early convergence, and renders the top-ranked symbols within a token budget. Result is cached in SQLite, keyed on `max(indexed_at)` -- invalidates automatically on any reindex.

## Performance engineering

- **mimalloc** global allocator
- **parking_lot::Mutex** instead of tokio::sync::Mutex (no async overhead on sync SQLite)
- **DashMap** for multi-tenant repo routing (sharded RwLock, zero contention on reads)
- **Fat LTO + panic=abort + opt-level=3** -- 24 MB binary
- **SQLite tuning** -- WAL, mmap 256MB, cache 32MB, PRAGMA optimize, chunked bulk inserts (500 files/tx)
- **Tantivy-first search** -- try BM25 index before falling back to grep, 50MB heap, commits every 20K docs
- **Token heuristic** -- estimate_tokens (len/4) in hot loops, tiktoken for accuracy checks
- **Map caching** -- PageRank cached in SQLite meta, invalidated on max(indexed_at) change
- **Early convergence** -- PageRank stops when L1 norm delta < 1e-6 (typically 8-12 iterations)
- **ahash::AHashMap** -- non-cryptographic hash in PageRank and RRF fusion hot paths
- **Bulk SQL** -- all_symbols() and all_refs() single-query loads for PageRank
- **Secret redaction** -- regex scanner on all tool responses returning code content
- **Path traversal guard** -- canonicalize + prefix check, repo root cached
- **Stdout hygiene** -- all logging to stderr, verified by CI test

## Testing

```bash
cargo test --workspace           # 107 tests across 19 suites
cargo clippy --workspace -- -D warnings
```

- Zero `unwrap()` in production library code
- `#[deny(missing_docs)]` on all 7 crate roots
- Secret redaction on all code-returning tool responses
- Stdout hygiene subprocess test
- Self-host E2E (index this repo, verify symbols)
- Incremental E2E (cold index, HEAD skip, merkle diff, delete cascade)
- Tool description length enforcement (<2 KB each)

## ADRs

- [000 -- Symbol-first architecture](docs/adr/000-symbol-first-architecture.md)
- [001 -- Text search backend](docs/adr/001-text-search-backend.md)
- [002 -- Output shape discipline](docs/adr/002-output-shape-discipline.md)
- [003 -- Stdio transport hygiene](docs/adr/003-stdio-transport-hygiene.md)

## License

MIT OR Apache-2.0
