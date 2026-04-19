# recon

Token-lean code intelligence MCP server for Claude Code.

Replaces `Read`, `Grep`, and `Glob` with symbol-aware tools that deliver **5-10x token reduction** on typical coding tasks.

## Features

- **10 `code_*` MCP tools** — outline, skeleton, read_symbol, find_symbol, find_refs, search, list, repo_map, find_strings, multi_find
- **9 languages** — Rust, Python, TypeScript, TSX, JavaScript, Go, Java, C, C++
- **Symbol-first architecture** — tree-sitter AST extraction, SQLite FTS5 trigram search, nucleo fuzzy matching
- **Incremental indexing** — blake3 content hashing, file watcher with 250ms debounce
- **Single binary** — no runtime dependencies, no cloud APIs, no GPU required
- **Stdio transport** — works with Claude Code, Cursor, Windsurf

## Install

```bash
cargo install --path crates/recon-cli
```

## Usage

### As MCP server (Claude Code)

Add to `.claude/settings.json`:

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

### Index a repo manually

```bash
recon index --repo /path/to/repo
```

### CLI

```bash
recon serve --repo .          # Start MCP server over stdio
recon index --repo .          # Index without serving
```

## Architecture

```
crates/
  recon-core/       # Shared types, errors, 5 output shapes
  recon-parser/     # Tree-sitter pools, symbol extraction
  recon-storage/    # SQLite schema, FTS5 trigram, blake3
  recon-search/     # Text search (grep-*), nucleo fuzzy
  recon-embed/      # Embeddings (future, feature-gated)
  recon-indexer/    # File walker, watcher, incremental index
  recon-server/     # rmcp MCP handler, 10 tools
  recon-cli/        # CLI binary
```

## Tools

| Tool | Replaces | Output |
|---|---|---|
| `code_outline(path)` | Read (orientation) | One line per symbol |
| `code_skeleton(path)` | Read (broad) | Signatures + docs, bodies elided |
| `code_read_symbol(path, symbol)` | Read (targeted) | Full source of one symbol |
| `code_find_symbol(name)` | Grep (symbols) | Qualified names + paths |
| `code_find_refs(symbol)` | Grep (usages) | Count + top-k sites |
| `code_search(query, mode)` | Grep (freeform) | Path + line + snippet |
| `code_list(glob?, lang?)` | Glob | Structured file listing |
| `code_repo_map(budget)` | — | Ranked symbol overview |
| `code_find_strings(pattern)` | — | String literal/comment search |
| `code_multi_find(patterns[])` | — | Multi-pattern single-pass search |

## License

MIT OR Apache-2.0
