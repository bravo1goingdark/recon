# recon

Token-efficient code intelligence for AI coding agents.

Recon indexes a local repository and exposes MCP tools that answer code questions without forcing an agent to read whole files or grep the tree. It extracts symbols and references, builds SQLite/Tantivy search indexes, maintains an incremental file snapshot, and serves structured `code_*` tools for navigation, search, repo maps, call graphs, and token-savings telemetry.

Links: [Website](https://mcprecon.pages.dev) · [Docs](https://mcprecon.pages.dev/Docs.html) · [Changelog](https://mcprecon.pages.dev/changelog)

## Why Recon

AI agents are expensive when they explore code with raw `Read`, `Grep`, and `Glob`. Those tools work, but they send too much text through the model.

Recon gives the agent indexed, symbol-aware answers instead:

- read one function instead of a whole file
- find a symbol through exact, BM25, FTS, fuzzy, and optional semantic tiers
- inspect callers, callees, call paths, and blast radius
- list files with language and symbol metadata
- build a PageRank-style repo map within a token budget
- track the tokens avoided by using structured tools

The result is a local MCP server that keeps code on the machine while giving agents a smaller, more useful view of the repo.

## What It Provides

Recon currently exposes 20 MCP tools:

| Area | Tools |
|---|---|
| File and symbol views | `code_outline`, `code_skeleton`, `code_read_symbol` |
| Search | `code_find_symbol`, `code_find_refs`, `code_search`, `code_find_strings`, `code_multi_find` |
| Repo orientation | `code_list`, `code_repo_map`, `code_stats`, `code_reindex` |
| Graph navigation | `code_path`, `code_callers`, `code_callees`, `code_context`, `code_impact`, `code_subsystems`, `code_subsystem` |
| Telemetry | `code_savings` |

Supported parser languages include Rust, Python, JavaScript, TypeScript, TSX, Go, Java, C, and C++.

## Quick Start

Install the binary:

```bash
curl -fsSL https://mcprecon.pages.dev/install.sh | bash
```

Authenticate once per machine:

```bash
recon login sk-recon-your-key
```

Index a project and wire it into your agent:

```bash
recon init --mcp cc        # Claude Code: .mcp.json
recon init --mcp oc        # OpenCode: .opencode/mcp.json
recon init --mcp cursor    # Cursor: .cursor/mcp.json
recon init --mcp windsurf  # Windsurf: ~/.codeium/windsurf/mcp_config.json
```

For indexing only:

```bash
recon init
```

Your MCP client starts `recon serve` automatically from the generated config. You normally do not need to run the server by hand.

## Common CLI Commands

```bash
recon license             # show cached tier, limits, and expiry
recon index               # index current repo without MCP wiring
recon stats               # show index health
recon find Handler        # find symbols by name
recon search "TODO"       # text search
recon map --budget 2000   # repo overview
recon purge --mcp cc      # remove index and Claude Code wiring
recon update              # update to latest release
recon logout              # remove cached license
```

## Configuration

Recon reads `.recon/config.toml` from the repo root. Missing values use secure defaults.

```toml
# Additional ignore patterns, on top of .gitignore and built-in vendor filters.
ignore_patterns = []

# Indexing and embedding limits.
max_file_size = 1048576
max_embed_size = 102400

# Runtime tuning.
watcher_debounce_ms = 250
tantivy_heap_bytes = 50000000
max_search_results = 30
default_map_budget = 2000

# Security defaults.
redact_secrets = true
allow_sensitive = false
```

Important distinction: product tiers and indexing options are separate.

- Tiers control account/resource limits such as max repos, max files, and max LOC.
- Config controls local indexing behavior such as file-size caps, secret redaction, and sensitive-file access.

Built-in tier presets:

| Tier | Repos | Files per repo | LOC per repo |
|---|---:|---:|---:|
| Free | 1 | 250 | 10K |
| Pro | 10 | 5K | 200K |
| Team | 25 | 50K | 4M |
| Enterprise | 1000 | unlimited | unlimited |

## Security Model

Recon is designed to keep source code local during normal MCP use.

- Tool path inputs are canonicalized and must stay inside the repo root.
- Sensitive files such as `.env` and private keys are blocked by default.
- Code-returning responses are secret-redacted by default.
- Vendored and generated files are filtered before indexing.
- Worker rate limits fail closed in production if required bindings are missing.
- Logging is kept off stdout so MCP JSON-RPC transport stays clean.

Semantic embedding is optional and can involve the hosted Worker/Modal path depending on configuration. Lexical and symbol search remain local.

## Architecture

```text
crates/
  recon-core/          shared types, config, errors, redaction, output shapes
  recon-parser/        tree-sitter parsing and symbol/reference extraction
  recon-storage/       SQLite storage, FTS, hashes, migrations
  recon-search/        Tantivy BM25, text search, fuzzy search, PageRank, tokens
  recon-embed/         local vector storage and embedding support
  recon-embed-client/  hosted embedding client
  recon-indexer/       repo walking, Merkle snapshots, incremental indexing, watcher
  recon-server/        MCP handlers, tools, multi-repo routing, telemetry
  recon-cli/           login, init, serve, index, query, savings, update

worker/                Cloudflare Worker API for auth, billing, dashboard,
                       license, savings, rate limits, and hosted embeddings

modal/                 Modal embedding service
site/                  marketing/docs/dashboard site
```

## Indexing Pipeline

1. Repo walk applies `.gitignore`, built-in vendor/generated filters, file-size limits, and sensitive-path policy.
2. Source files are parsed in parallel with pooled tree-sitter parsers.
3. Symbols, references, file metadata, and hashes are stored in SQLite.
4. Tantivy builds the structured BM25 index.
5. A Merkle snapshot tracks file content and mtimes for incremental reindexing.
6. A live watcher debounces file changes and reindexes changed files.

Cold starts do a full parse. Later runs skip unchanged repos or reindex only changed/deleted files.

## Search Pipeline

Recon combines several retrieval paths:

| Layer | Backend | Purpose |
|---|---|---|
| Exact symbol | SQLite indexes | fast known-name lookup |
| Structured text | Tantivy BM25 | ranked code search |
| FTS/fuzzy | SQLite FTS5 + nucleo | typo-tolerant symbol matching |
| Raw text | grep/regex backend | exact and regex search |
| Semantic | embedding service/client | optional meaning-based fallback |

Graph tools use the same symbol/reference substrate and build forward/reverse adjacency structures for bounded BFS over callers, callees, paths, and impact.

## Telemetry and Dashboard

Recon tracks per-tool counters locally:

- calls
- response tokens
- baseline tokens avoided
- tokens saved
- latency

`code_savings` returns the local totals. `recon savings show` prints them from the CLI. Pro/Team users can push daily rollups to the dashboard:

```bash
recon savings push
```

The dashboard payload contains aggregate counters only. It does not send source code, file paths, symbol names, or query strings.

## Development

Rust workspace checks:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

Worker checks:

```bash
cd worker
npm run typecheck
npm test
```

The current suite includes Rust unit/integration tests across the workspace and a Cloudflare Worker Vitest suite covering auth, billing, dashboard, embeddings, and rate-limit behavior.

## Documentation

- [Architecture](docs/ARCHITECTURE.md)
- [Performance baseline](docs/PERF_BASELINE.md)
- [FFF integration](docs/FFF_INTEGRATION.md)
- [Hosted embedding plan](docs/HOSTED_EMBED_PLAN.md)
- [ADR 000: Symbol-first architecture](docs/adr/000-symbol-first-architecture.md)
- [ADR 001: Text search backend](docs/adr/001-text-search-backend.md)
- [ADR 002: Output shape discipline](docs/adr/002-output-shape-discipline.md)
- [ADR 003: Stdio transport hygiene](docs/adr/003-stdio-transport-hygiene.md)

## License

MIT OR Apache-2.0
