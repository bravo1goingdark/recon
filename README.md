# recon

Token-lean code intelligence MCP server. Replaces `Read`, `Grep`, and `Glob` with symbol-aware tools that deliver **15-30x token reduction** on code exploration tasks.

Offered as a **hosted MCP server** -- no binary to install, no local indexing. Point your agent at the endpoint, hand it an API key, and it gets 12 symbol-aware tools instantly.

## Benchmarks

Measured on real codebases, release build, warm cache:

| | Zed (80K symbols) | Rust compiler (318K symbols) |
|---|---|---|
| **stats** | 11 ms | 28 ms |
| **find** | 10 ms | 11 ms |
| **search** | 14-39 ms | 33-95 ms |
| **outline** | 16 ms | 20 ms |
| **skeleton** | 18 ms | 16 ms |
| **refs** | 16 ms | 9 ms |
| **map (cached)** | 8 ms | 18 ms |
| **map (cold)** | 405 ms | 2.0 s |
| **reindex** | 31 s | -- |

All read-path queries under 100 ms p99. Binary size: 24 MB.

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

## Self-hosted setup

If you run your own server:

```bash
# One-command setup: indexes repo, writes .mcp.json, updates .gitignore
recon init

# Or manual
recon index                     # index the repo
recon serve                     # start MCP server over stdio
recon serve --port 3100         # start over Streamable HTTP
```

### Multi-tenant hosted server

```bash
# Create a keys.json
cat > keys.json << 'EOF'
{
  "keys": {
    "sk-customer-a": "/srv/repos/customer-a",
    "sk-customer-b": "/srv/repos/customer-b"
  }
}
EOF

# Start the hosted server with API key auth
recon serve-hosted --keys keys.json --port 3100
```

Requests without a valid `Authorization: Bearer <key>` header get `401`. Invalid keys get `403`. Each key routes to its own isolated repo index via DashMap.

## CLI (`rr` -- recon remote)

Public CLI client for querying hosted recon servers. No local deps, just HTTP:

```bash
cargo install rr

# Set server URL
export RECON_URL=https://mcp.recon.dev/v1

# Query
rr find TyCtxt
rr search 'fn render'
rr outline src/main.rs
rr skeleton src/lib.rs
rr refs Editor
rr map --budget 2000
rr stats
rr ping                         # check connectivity
rr update                       # self-update from GitHub Releases
```

All output is human-readable by default. Pass `--json` for machine consumption.

## Tools (12)

| Tool | Replaces | What it does | Latency |
|---|---|---|---|
| `code_outline(path)` | Read | One line per symbol -- kind, name, line | <20 ms |
| `code_skeleton(path)` | Read | Signatures + docs, bodies as `...` (10x compression) | <20 ms |
| `code_read_symbol(path, symbol)` | Read | Full source of one symbol + callers | <10 ms |
| `code_find_symbol(name)` | Grep | 3-tier: exact SQLite -> Tantivy BM25 -> FTS5 + nucleo fuzzy | <15 ms |
| `code_find_refs(symbol)` | Grep | Reference count + top-k call sites | <50 ms |
| `code_search(query, mode, filter?)` | Grep | exact/regex/hybrid + filter DSL, Tantivy-first | <100 ms |
| `code_list(glob?, lang?, filter?)` | Glob | Structured file listing with symbol counts | <30 ms |
| `code_repo_map(budget)` | -- | PageRank-ranked symbol overview under token budget | <20 ms cached |
| `code_find_strings(pattern)` | -- | Search string literals and comments | <30 ms |
| `code_multi_find(patterns[])` | -- | Multi-pattern search in one call | <30 ms |
| `code_reindex()` | -- | Agent-triggered re-indexing | varies |
| `code_stats()` | -- | Index health: files, symbols, freshness | <30 ms |

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
  recon-cli/        # CLI: serve, serve-hosted, init, index, purge, query tools
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
- **SQLite tuning** -- WAL, mmap 256MB, cache 32MB, PRAGMA optimize
- **Tantivy-first search** -- try BM25 index before falling back to grep
- **Token heuristic** -- estimate_tokens (len/4) in hot loops, tiktoken for accuracy checks
- **Map caching** -- PageRank cached in SQLite meta, invalidated on max(indexed_at) change
- **Early convergence** -- PageRank stops when L1 norm delta < 1e-6 (typically 8-12 iterations)
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
