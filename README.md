# recon

Token-lean code intelligence MCP server. Replaces Claude Code's `Read`, `Grep`, and `Glob` with symbol-aware tools.

## Performance

| Metric | Value |
|---|---|
| Cold index (30 files, 449 symbols) | 119ms |
| Binary size | 22MB (LTO, stripped) |
| Languages | 9 (Rust, Python, TypeScript, TSX, JavaScript, Go, Java, C, C++) |
| Search tiers | Exact SQLite -> Tantivy BM25 -> FTS5 trigram + nucleo fuzzy |
| Index freshness | <1s via file watcher (notify, 250ms debounce) |

## Install

```bash
cargo install --path crates/recon-cli
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

To get maximum token savings, add a `CLAUDE.md` rule in your project:

```markdown
Use `code_*` tools (code_outline, code_skeleton, code_find_symbol, code_search,
code_repo_map) before Read/Grep/Glob when exploring code. They return structured,
token-efficient results.
```

## Usage

```bash
recon serve --repo .          # Start MCP server over stdio (with live file watching)
recon index --repo .          # Index without serving
```

## Tools (12)

| Tool | Replaces | What it does |
|---|---|---|
| `code_outline(path)` | Read | One line per symbol — kind, name, line |
| `code_skeleton(path)` | Read | Signatures + docs, bodies as `...` (10x compression) |
| `code_read_symbol(path, symbol)` | Read | Full source of one symbol + callers |
| `code_find_symbol(name)` | Grep | 3-tier search: exact -> BM25 -> fuzzy |
| `code_find_refs(symbol)` | Grep | Reference count + top-k call sites |
| `code_search(query, mode)` | Grep | exact/regex/hybrid (BM25+text RRF fusion) |
| `code_list(glob?, lang?)` | Glob | Structured file listing with top symbols |
| `code_repo_map(budget)` | — | PageRank-ranked symbol overview |
| `code_find_strings(pattern)` | — | Search string literals and comments |
| `code_multi_find(patterns[])` | — | Multi-pattern single-pass search |
| `code_reindex()` | — | Agent-triggered re-indexing |
| `code_stats()` | — | Index health report |

## Architecture

```
crates/
  recon-core/       # Types, errors, 5 output shapes, config, secret redaction
  recon-parser/     # Tree-sitter pools (9 langs), symbol extraction
  recon-storage/    # SQLite + FTS5 trigram, blake3, batch inserts
  recon-search/     # Tantivy BM25, grep-* text search, nucleo fuzzy, PageRank
  recon-embed/      # Embeddings (future, feature-gated)
  recon-indexer/    # Rayon parallel parse, file watcher, incremental index
  recon-server/     # rmcp MCP handler, 12 tools, secret redaction
  recon-cli/        # CLI binary (serve + index)
```

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

- **Rayon parallel parsing** — tree-sitter across all CPU cores
- **LanguagePools** — parser reuse across threads (no per-file creation)
- **Batch SQLite inserts** — single transaction per file (symbols + refs)
- **prepare_cached** — all queries use cached prepared statements
- **SQLite tuning** — WAL, mmap_size=256MB, cache_size=8000, temp_store=MEMORY
- **Tantivy BM25** — CodeSplitTokenizer for camelCase/snake_case recall
- **Hybrid search** — reciprocal rank fusion of Tantivy + text results
- **PageRank repo-map** — Aider-style personalized ranking with edge weights
- **Streaming blake3** — memmap2 for files >64KB
- **LTO thin + codegen-units=1** — optimized release binary

## License

MIT OR Apache-2.0
