# recon

Token-lean code intelligence MCP server. Replaces Claude Code's `Read`, `Grep`, and `Glob` with symbol-aware, structure-first tools that deliver 5-10x token reduction on typical coding tasks.

## Performance

| Metric | Measured |
|---|---|
| Cold index (44 files, 638 symbols) | 156 ms |
| Incremental skip (HEAD unchanged) | 6 ms |
| Merkle diff (1 file changed) | 39 ms |
| Symbol exact lookup (10K symbols) | 25 us |
| Symbol fuzzy search (10K symbols) | 150 us |
| Tantivy BM25 query (1K symbols) | 10 us |
| Text grep (50 files, fff-grep) | 377 us |
| Repo map render (500 symbols) | 3.4 ms |
| Token counting (tiktoken cl100k) | 55 us |
| Batch insert (1K symbols) | 46 ms |
| Binary size (release, stripped) | 26 MB |
| Index size vs repo | <1% at scale |
| Languages | 9 (Rust, Python, TypeScript, TSX, JavaScript, Go, Java, C, C++) |
| Index freshness | <1s via notify watcher (250ms debounce) |

All read-path operations are well under the 100 ms p99 target.

## Install

```bash
# From source
cargo install --path crates/recon-cli

# Or via install script (downloads prebuilt binary)
curl -sL https://raw.githubusercontent.com/bravo1goingdark/recon/main/scripts/install.sh | bash
```

## Setup with Claude Code

Add to your project's `.claude/settings.json`:

```json
{
  "mcpServers": {
    "recon": {
      "command": "recon",
      "args": ["serve", "--repo", "."]
    }
  }
}
```

Add a `CLAUDE.md` rule in your project for maximum token savings:

```markdown
Use `code_*` tools (code_outline, code_skeleton, code_find_symbol, code_search,
code_repo_map) before Read/Grep/Glob when exploring code. They return structured,
token-efficient results.
```

## Usage

```bash
recon serve --repo .          # Start MCP server over stdio (with live file watching)
recon index --repo .          # Index without serving (incremental via Merkle diff)
```

## Tools (12)

| Tool | Replaces | What it does |
|---|---|---|
| `code_outline(path)` | Read | One line per symbol -- kind, name, line |
| `code_skeleton(path)` | Read | Signatures + docs, bodies as `...` (10x compression) |
| `code_read_symbol(path, symbol)` | Read | Full source of one symbol + callers |
| `code_find_symbol(name)` | Grep | 3-tier: exact SQLite -> Tantivy BM25 -> FTS5 + nucleo fuzzy |
| `code_find_refs(symbol)` | Grep | Reference count + top-k call sites |
| `code_search(query, mode, filter?)` | Grep | exact/regex/hybrid/semantic + filter DSL |
| `code_list(glob?, lang?, filter?)` | Glob | Structured file listing with top symbols |
| `code_repo_map(budget)` | -- | PageRank-ranked symbol overview (tiktoken-budgeted) |
| `code_find_strings(pattern, filter?)` | -- | Search string literals and comments |
| `code_multi_find(patterns[], filter?)` | -- | Multi-pattern search via TextSearcher trait |
| `code_reindex()` | -- | Agent-triggered re-indexing |
| `code_stats()` | -- | Index health report |

### Filter DSL

Search tools accept an optional `filter` parameter powered by fff-query-parser:

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
  recon-search/     # Tantivy BM25, fff-grep text search, nucleo fuzzy, PageRank
  recon-embed/      # fastembed + LanceDB vector search (feature-gated)
  recon-indexer/    # Merkle tree, gix ColdStart, file watcher, rayon parallel parse
  recon-server/     # rmcp MCP handler, 12 tools, FffBackend, filter DSL
  recon-cli/        # CLI binary (serve + index)
```

### Search tiers

| Tier | Backend | Latency |
|---|---|---|
| T0 -- Symbol exact | SQLite btree index | <1 ms |
| T1 -- Symbol fuzzy | SQLite FTS5 trigram + nucleo rescore | 2-8 ms |
| T2 -- Structured BM25 | Tantivy with CodeTokenizer (camelCase/snake_case split) | 5-15 ms |
| T3 -- Raw text/regex | fff-grep (SIMD + memmap2) via TextSearcher trait | 3-20 ms |
| T4 -- Semantic | fastembed + LanceDB (feature-gated, `--features embed`) | 50-150 ms |

### Incremental indexing

1. **ColdStart**: gix reads HEAD SHA -- if it matches the last indexed commit, skip entirely (6 ms).
2. **Merkle diff**: blake3 hash tree built from file contents. On HEAD change, diff against the previous snapshot and reindex only changed files (39 ms for 1 file).
3. **Full index**: first run or missing snapshot, parallel parse via rayon (156 ms for 44 files).
4. **Live watcher**: notify-debouncer-full (250 ms debounce) triggers per-file reindex. On overflow, falls back to gix status.

## Configuration

Create `.recon/config.toml` in your repo root:

```toml
# Additional ignore patterns (on top of .gitignore)
ignore_patterns = ["*.generated.*", "dist/"]

# Max file size to index (default 1MB)
max_file_size = 1048576

# Max search results per tool call (default 30)
max_search_results = 30

# Token budget for code_repo_map (default 2000)
default_map_budget = 2000

# Enable secret redaction (default true)
redact_secrets = true

# Allow .env/.pem/.key files (default false)
allow_sensitive = false
```

## Optimization highlights

- **Rayon parallel parsing** -- tree-sitter across all CPU cores
- **LanguagePools** -- parser reuse via ArrayQueue (no per-file creation overhead)
- **Batch SQLite inserts** -- single transaction per file (symbols + refs)
- **prepare_cached** -- all queries use cached prepared statements
- **SQLite tuning** -- WAL, mmap_size=256MB, cache_size=8000, temp_store=MEMORY
- **Tantivy BM25** -- CodeSplitTokenizer for camelCase/snake_case recall
- **fff-grep** -- SIMD-accelerated text search via memmap2, pinned to =0.4.0
- **TextSearcher trait** -- backend-agnostic search interface (FffBackend default, GrepBackend fallback)
- **Hybrid RRF** -- reciprocal rank fusion of Tantivy + text results
- **PageRank repo-map** -- Aider-style personalized ranking with edge weights
- **tiktoken-rs** -- accurate cl100k_base token counting (replaces len/4 heuristic)
- **Merkle snapshots** -- blake3 hash tree for O(changed) incremental reindexing
- **gix ColdStart** -- skip reparse entirely when HEAD is unchanged
- **Parser hot path** -- cached PathBuf, reusable qname buffer, SmallVec doc lines, pre-sized vectors
- **Streaming blake3** -- memmap2 for files >64KB
- **LTO thin + codegen-units=1** -- optimized release binary
- **Secret redaction** -- 13 regex patterns + PEM blocks + blocked paths on every response
- **Path traversal guard** -- canonicalize + prefix check on every tool call
- **Stdout hygiene** -- all logging to stderr, verified by CI test

## Testing

```bash
cargo test --workspace        # 110 tests (unit + integration + E2E)
cargo bench --workspace       # criterion benchmarks (storage, search, parser)
```

Test coverage:
- Unit tests per module (core, storage, parser, search, embed, indexer)
- Fixture-based parser tests (9 languages)
- Stdout hygiene subprocess test (JSON-RPC validation)
- Self-host E2E (index this repo, verify symbols)
- Incremental E2E (cold index, HEAD skip, merkle diff, delete cascade, multi-lang)
- Tool description length enforcement (<2 KB each)

## ADRs

- [000 -- Symbol-first architecture](docs/adr/000-symbol-first-architecture.md)
- [001 -- Text search backend](docs/adr/001-text-search-backend.md)
- [002 -- Output shape discipline](docs/adr/002-output-shape-discipline.md)
- [003 -- Stdio transport hygiene](docs/adr/003-stdio-transport-hygiene.md)

## License

MIT OR Apache-2.0
