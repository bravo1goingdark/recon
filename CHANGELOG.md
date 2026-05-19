# Changelog

All notable changes to this project are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the
project uses [SemVer](https://semver.org/).

## [Unreleased]

### Fixed

- Sensitive-path blocking now canonicalizes symlink targets before
  applying the denylist, closing alias-based bypasses and making blocked
  path checks fail closed when canonicalization or parsing fails.
- `recon serve` file-count startup checks now honor
  `.recon/config.toml` `ignore_patterns`, so configured ignored paths do
  not count against tier limits.
- Incremental reindex now detects changed `ignore_patterns` and
  cascade-deletes symbols/refs for already-indexed paths that are newly
  ignored, preventing stale search and repo-map entries.
- OAuth state handling and stale index cleanup were fixed in the web
  auth flow, including the related index maintenance path from the
  latest merged PR.
- Parallel test flakes caused by shared Codex environment variables were
  serialized to keep CI stable.

### Changed

- Repo startup and indexing flows now thread the configured file-size,
  sensitive-path, and ignore-pattern rules through the same validation
  paths.
- The CLI and server now use the same ignore-aware repository walker for
  file-count gating, keeping serve-time behavior consistent.
- Modal embed reliability improved with smaller batches, allocator
  tuning, retry backoff with jitter, and supporting CLI/site updates.
- Rust toolchain metadata now includes `rustfmt` and `clippy`
  components for CI and local checks.

## [0.5.6] — 2026-05-12

Operational hardening release. This patch closes the main regressions
found after the hosted-embeddings repair and makes savings telemetry
scale to the repository actually being indexed.

### Added — per-repo baseline calibration

- `index_repo()` now schedules a background calibration pass after the
  initial index. It simulates the alternative Read+Grep loops against
  the repo's own source files, persists per-tool median token counts in
  SQLite meta, and hot-reloads them into the current session.
- `Telemetry::baseline_for_local()` prefers those calibrated baselines
  for static-baseline tools, falling back to the shipped table until
  calibration has completed. Repos much larger than this one no longer
  inherit understated static savings forever.
- Calibration is bounded, delayed by 2 seconds so first tool calls are
  not starved, and re-runs when the indexed file count changes by more
  than 25% or after `code_reindex --force`.

### Fixed — multi-repo and tool correctness

- `restore_session()` no longer overrides the repo passed to
  `recon serve --repo`. Session restore now only preloads previously
  loaded repos so later `code_activate_repo` calls are fast; the startup
  repo remains active.
- Cache refresh gating moved from a process-global static to each
  `ReconServer`, allowing multi-repo servers to refresh independently.
- Path-based tools now canonicalize relative paths such as
  `src/../src/math.rs` back to the indexed relative path before lookup.
- `sk-recon-*` API keys are allowlisted in response redaction so
  user-facing login/help messages do not replace recon's own keys with
  `***REDACTED***`.
- Env-var-mutating tests now serialize access to avoid parallel-test
  races in embed-client, router, and pretty-output coverage.

### Changed — indexing, config, and release hygiene

- Indexing now honors `.recon/config.toml` limits consistently:
  `max_file_size`, `tantivy_heap_bytes`, and `allow_sensitive` are
  threaded through full, incremental, Merkle, watcher, and manual
  reindex paths.
- Sensitive paths (`.env*`, private-key material, and blocked key
  extensions) are skipped during indexing unless
  `allow_sensitive = true` is explicitly set.
- Worker rate-limit bindings fail closed in production when missing,
  while local/test requests still fail open for development.
- CI/deploy/release workflows were updated for the current Node runtime,
  and the README was refreshed to match the current product surface.

### Removed

- Automatic savings push on `recon serve` shutdown has been removed.
  Dashboard uploads are explicit again via `recon savings push`, which
  avoids surprising network side effects at session exit.

## [0.5.5] — 2026-05-02

Hosted-embeddings repair release. `mode: "semantic"` was failing
closed for every user — credentials valid, key authorized,
worker reachable, but three independent bugs along the embed
path stopped vectors from reaching the vector store.

### Fixed — semantic search end-to-end

- **`router::load_repo` now initializes the embed service.** Any
  repo activated through the multi-repo router (i.e. via
  `code_activate_repo`) constructed a fresh `ReconServer` whose
  `embed_service` stayed `None`, so semantic queries against
  router-managed repos returned the "requires the embed service"
  sentinel even though the primary `--repo` worked. Mirror the
  startup flow: warn-on-fail, never fatal.
- **`init_embed` creates `.recon/vectors/` before opening
  `VectorStore`.** rusqlite refuses to open a db whose parent
  directory does not exist, and `init_embed` did not
  `create_dir_all`. The result was a silent failure on every
  fresh repo, primary path included — the embed service was
  built, the vector store open call errored, and the caller
  warned without setting `embed_service`. `create_dir_all`
  resolves it.
- **`recon-embed-client` `DEFAULT_API_URL` now points at the
  deployed worker host.** The hardcoded default was
  `https://api.mcprecon.dev`, which has no DNS record (NXDOMAIN),
  so every hosted embed call failed with a transport error
  mapped to `Error::EmbedUnavailable`. The rest of the codebase
  (`recon-server::license`, `recon-cli::savings`) already used
  the workers.dev hostname that actually serves traffic;
  aligning here closes the gap. `RECON_API_URL` continues to
  override.

### Changed — minor

- `ReconServer::init_embed` is no longer `async`; its body had
  no `.await` and the sync signature lets `router::load_repo`
  (sync, inside a dashmap entry block) call it without an
  executor handle. Sole async caller in `recon-cli` drops
  `.await`.
- Field `ReconServer::embed_service` is `pub(crate)` so the new
  cross-module regression test in `router.rs` can assert it
  becomes `Some` after a router load.

## [0.5.4] — 2026-05-02

Token-savings credibility release. Every number in the static
`BASELINES` table now traces to either real BPE on real file content
(4 migrated tools) or a `cargo run` you can rerun yourself (9
non-migrated tools via the new `bench-baselines` binary). The headline
"tokens saved" figure is no longer a vendor metric — it's an audit
trail.

### Changed — measured baselines replace asserted ones

- New `crates/recon-cli/src/bin/bench-baselines.rs` walks a fixture
  repo, simulates each non-migrated tool's literal Read+Grep
  alternative, and BPE-counts the result via `count_tokens`. Static
  `BASELINES` rows now carry the resulting medians + (low, high)
  ranges. Previous asserted point estimates under-claimed the real
  alternative-loop cost by 2–5×.
- `Baseline` struct extended with `range_low_tokens`,
  `range_high_tokens`, and `derivation` — a one-line reproducible
  methodology string. `code_savings` emits a "methodology" trailer
  listing each used tool's range + derivation so dashboard consumers
  can audit at a glance.
- `ENCODER_VERSION` bumped `bpe-v1` → `bpe-v2-baselines-measured`.
  First hydrate after upgrade drops the persisted token counters
  (response, static_baseline, measured_baseline) so old asserted-unit
  history doesn't silently mix with new measured-unit credit on the
  dashboard. `calls` and `latency_micros_total` carry over unchanged.

### Performance — measured-baseline path hardened

- **Real BPE replaces char/4 on the file-content baseline.** The four
  migrated tools (`code_outline`, `code_skeleton`, `code_read_symbol`,
  `code_context`) now BPE-count the file the agent would have read
  via cl100k_base. `count_tokens_capped` bounds per-call cost at 32 KB
  with linear extrapolation for larger files; the encode runs off the
  tokio executor via `spawn_blocking`. Cache is shared across all four
  tools so a session that touches the same file from multiple handlers
  pays one BPE pass.
- **`recon_search::tokens::prewarm()`** at server construction loads
  the cl100k_base merge table during startup so the first MCP call
  doesn't pay the ~100 ms initialisation tax.
- **`response_tokens` stays heuristic** on the hot path (real BPE here
  would add ~1 ms per MCP call; measurable for sub-millisecond tools
  like `code_outline`). To close the unit asymmetry between baseline
  and response, `Telemetry::sample_response` runs a fire-and-forget
  BPE sample on 1-in-64 calls (responses ≥ 1 KB), tracking a running
  BPE/heuristic ratio. `code_savings` applies the ratio to display
  corrected `tokens_saved`.
- **Per-process baseline-credit dedup persists to SQLite** with a 24 h
  sliding-window TTL. A `recon serve` restart inside the window no
  longer re-credits every file's baseline, closing the lifetime
  inflation hole.
- **True LRU eviction** on `measured_baseline_cache` (was iteration-order
  eviction).

### Caveat

The static `BASELINES` integers are calibrated against this repo
specifically (130 source files, ~MB scale). Repos of dramatically
different shape — e.g. the rust-lang/rust source tree, where the
alternative loop costs 20–80× more tokens — will see understated
savings until per-repo background calibration lands (tracked as
GitHub issue #29).

## [0.5.3] — 2026-04-30

Performance + semantic-by-default release. Hosted semantic search now
works in the released default-features binary (previously gated behind
a feature flag that nobody could opt into); SQLite + Tantivy hot paths
got a sweep of fixes that knock real time off cold-index and hot tool
calls.

### Changed — hosted semantic search wired in by default

`init_embed()` was gated behind `#[cfg(feature = "embed")]`, so the
released default-features binary never instantiated `HostedEmbedService`
and `code_search mode=semantic` failed closed even with valid
credentials. Since this repo is private and end users run the prebuilt
binary, that gate effectively disabled hosted semantic for everyone.
Ungated; the misleading `embed = ["local-embed"]` alias is gone.
`local-embed` remains as the explicit air-gapped opt-in.

### Performance — storage / search / server

- **`(path, byte_start)` compound index — schema V5.** `symbols_for_path`
  did `WHERE path = ?1 ORDER BY byte_start`; the V1 single-column
  `symbols_path` index forced an index seek + filesort. The compound
  index makes the planner walk rows in already-sorted order, so every
  `code_outline` / `code_skeleton` / `code_read_symbol` call on a
  many-symbol file pays no sort step. **Sub-millisecond on a 335K-symbol
  index.**
- **Drop FTS triggers during cold bulk index, rebuild after.** Each
  `INSERT INTO symbols` previously fired the `symbols_ai` trigger,
  which tokenised name + qualified_name + signature, computed
  trigrams, and updated FTS5's internal btree — tens of millions of
  trigram operations synchronously inside the bulk-insert transaction.
  `enter_indexing_mode` now drops the three FTS triggers; `exit_indexing_mode`
  issues one batched `INSERT INTO symbols_fts(symbols_fts) VALUES('rebuild')`
  and recreates the triggers. `Store::init` runs a defensive
  `repair_fts_state_if_needed` so an aborted bulk-index run can't leave
  the FTS index half-built.
- **Single-flight gate on `cached_call_graph`.** Two parallel `code_path` /
  `code_callers` / `code_callees` / `code_impact` requests landing during
  a cold-cache window each rebuilt the CSR call graph (~50–200 ms on a
  300K-symbol repo) and the later `store()` overwrote the earlier one.
  Added a `parking_lot::Mutex<()>` gate around the build phase with a
  double-check after the lock so a waiter that arrived after the build
  completed sees the published value and skips its own rebuild.
- **Drop interim Tantivy commits during cold index.** Cold-index path
  issued a commit every 20K symbols; each commit publishes a new
  segment, and search latency scales with segment count. A 300K-symbol
  cold index produced ~15 segments. One commit at the end produces
  ~1–2; Tantivy's writer heap (50 MB) already flushes internal
  segments without publishing them.
- **Telemetry flush — release lock before SQLite writes.**
  `flush_to_store` held a `parking_lot::Mutex` across the full
  `set_meta()` loop, serialising concurrent flushes behind disk I/O for
  the entire critical section. Snapshot the per-tool counters under
  lock (one atomic load per counter), drop the guard, then issue the
  writes outside it. Critical section drops from ~ms to ~µs.
- **Bound watcher channel to 64 batches.** notify-debouncer feed was
  unbounded (std `mpsc::channel`); a slow consumer or stalled parser
  could let the queue grow without limit. `sync_channel(64)` provides
  observable backpressure (~16 s of continuous churn before the
  debouncer thread parks).
- **Prepared-statement cache cap 16 → 128.** rusqlite's default LRU is
  16; the hot read path issues ~25 distinct `prepare_cached()` statements
  and the writer ~30. Cap raised to 128 on every connection (writer,
  read pool, embed-side read pool at 64). Eliminates re-prepare overhead
  on a normal tool-call mix.

### Test tooling

- `bench-real` parses `code_list`'s Hits-shape response correctly (used
  to panic mid-bench).
- `RECON_BENCH_KEEP_INDEX=1` skips the `.recon/` wipe so a re-run after
  a downstream panic reuses the warm index.

### CI

- Release / cross-platform workflows reference `--features local-embed`
  (the `embed` alias is gone). The feature jobs are renamed
  `test-local-embed` / `local-embed` accordingly.

## [0.3.4] — 2026-04-29

Hardening release. Three concurrency / process-supervision bugs surfaced
during normal multi-tool development against a long-running
`recon serve`; all three fixed end-to-end.

### Fixed — `e2e_self_host` no longer wipes the live workspace `.recon/`

`crates/recon-cli/tests/e2e_self_host.rs` resolved its target with
`CARGO_MANIFEST_DIR.parent().parent()`, which evaluates to the actual
working repo root, then ran `fs::remove_dir_all(workspace_root.join(".recon"))`
twice (entry + exit). For anyone running `cargo test --workspace`
while a `recon serve` was open against the same checkout (Claude Code
/ Cursor / Windsurf during development), the test silently unlinked
files the running server had open file descriptors on. The kernel
kept the inode alive for the open fds; the on-disk path was
recreated by the test's own `recon index` run; the running server kept
operating against the orphaned (deleted) inode while everything else
on disk pointed at a fresh inode. Silent split-brain — visible only
via `ls -l /proc/<pid>/fd | grep deleted`.

The test now copies a representative subset of the workspace
(`Cargo.toml`, `Cargo.lock`, `crates/recon-core/`, `crates/recon-storage/`)
into a `tempfile::TempDir` and indexes that. The tempdir is
auto-cleaned on Drop. The live workspace `.recon/` is never touched.
A regression-guard `assert_ne!` on the path itself locks the new
behavior in.

### Fixed — `recon serve` self-heals when its `.recon/index.db` is unlinked

Even with the test fixed, the same failure mode can be triggered by
any `rm -rf .recon/`, container restart racing with a still-running
server, or a botched manual recovery. `start_watcher` now captures
the file id of `.recon/index.db` at startup and re-checks it on
each 500 ms recv-timeout cycle of the watcher loop. If it diverges
(file unlinked, or replaced by a new file at the same path), the
server logs a warning, sets the shutdown flag, wakes the
`tokio::sync::Notify` that v0.3.2 added for license-revocation
shutdowns, and exits cleanly. The IDE supervisor (Claude Code /
Cursor / Windsurf / opencode) sees the child exit and respawns it;
the new child opens the live file on startup and operates correctly.
Cost on the hot path is one `stat()` per 500 ms — ~3 µs on Linux
ext4.

Cross-platform: a small `file_id` helper resolves to Unix inode
(`std::os::unix::fs::MetadataExt::ino`) on Linux/macOS and NTFS
file-index (`std::os::windows::fs::MetadataExt::file_index`) on
Windows. Modern SQLite opens with `FILE_SHARE_DELETE` on Windows so
the unlink-while-open scenario is reachable there too — file is
marked for deletion, lingers until our last handle closes, the
on-disk path can be recreated by another process at a fresh
file-index. Same shape, same fix. Other platforms (wasi, redox,
fuchsia, …) get `None` and the check no-ops; no behavior change
from v0.3.3 there.

### Fixed — `server.shutdown()` no longer wedges on a phantom DB

When the underlying `index.db` was unlinked (the failure mode above
in either form), the synchronous WAL flush in
`Telemetry::flush_sync` and `Store::exit_indexing_mode` blocked on
the orphaned inode forever — SIGTERM was ignored, only SIGKILL
worked. Real impact: systemd `kill --signal=TERM`, Docker
`stop --time=10`, and Kubernetes graceful-shutdown windows all
expire and force-kill, losing any unflushed telemetry that *would*
have flushed if shutdown had returned in bounded time.

Both serve paths (stdio + HTTP) now wrap `server.shutdown().await`
in `tokio::time::timeout(5s, ...)` via the new
`shutdown_with_timeout` helper. If the deadline fires we log a
warning naming the most likely cause (unlinked `.recon/`) and let
the process exit anyway. Healthy shutdowns finish in well under a
second on the largest indexes we've measured; 5 s is the
pathological-case ceiling.

### Tests

- `e2e_self_host::index_self_and_verify_symbols` rewritten against a
  tempdir snapshot; new helpers `copy_tree` and
  `build_isolated_workspace_snapshot` factor the source-copy logic.
  Verified end-to-end: `cargo test --workspace` now leaves the live
  `.recon/` mtime untouched.
- All 491 prior workspace tests still pass; cargo fmt clean; clippy
  `-D warnings` clean.

### Fixed — release pipeline `latest.json` step (regression from v0.3.3)

The v0.3.3 release run (run 25097548024, "Update latest.json (only if
this is the highest tag)" step) failed silently with exit 1 in 600 ms,
no captured diagnostic. Root cause: the `Publish to R2` job's
`actions/checkout@v4` was at default `fetch-depth: 1`, so the
follow-up `git fetch --tags --quiet` had to fetch every tag from
scratch — and `--quiet` swallowed whatever transient error the fetch
hit, leaving `set -e` to kill the script with no message.

The release binaries themselves uploaded successfully (the failing
step ran *after* `Upload binaries + sums + signatures to R2`), but
`latest.json` on R2 stayed pinned to v0.3.2 — meaning
`recon update --check` would still see v0.3.2 as the latest released
version even after v0.3.3 was tagged.

Fix: pin the publish job's checkout to `fetch-depth: 0` so all tag
refs are already local. Drop the redundant `git fetch --tags`. Add
`set -euo pipefail` and an `echo` of the resolved tag list so any
future failure has a paper trail.

The v0.3.4 release run (this commit's tag) will retake `latest.json`
to v0.3.4 because v0.3.4 sorts higher than every previously-released
tag — so the v0.3.3 hangover heals itself the moment v0.3.4 ships.

### Migration notes

- **CLI**: rebuild + reinstall. The Fix #2 self-heal and Fix #3
  shutdown timeout only land in the new binary — existing v0.3.3
  installs are still vulnerable to the orphaned-inode trap in any
  scenario where `.recon/` is unlinked under them. (After v0.3.4
  installs, an unlink triggers a clean restart instead of silent
  drift.)
- **Worker / D1 / dashboard**: no changes in v0.3.4 — no migration,
  no `wrangler deploy` needed. Schema is untouched from v0.3.3.
- **`recon update`**: previously read `latest.json` and got v0.3.2
  even after v0.3.3 was published. The v0.3.4 release fixes the
  pipeline AND overrides the manifest in the same run, so users on
  v0.3.2 / v0.3.3 will see v0.3.4 as the next available version.

[0.3.4]: https://github.com/bravo1goingdark/recon/releases/tag/v0.3.4

## [0.3.3] — 2026-04-29

CLI + MCP correctness sweep. Watcher → query loop now async-refresh.
Savings dashboard now aggregates true cross-repo totals.

### Changed — savings dashboard SUMs across repos (was MAX)

The v0.3.2 savings dashboard keyed `usage_rollups` on `(user_id, day)`,
so multi-repo users (Pro tier allows up to 10) had every push from every
repo MAX-merged into one bucket. The headline was the high-water mark
of one repo, not the cross-repo total — accurate per-row but misleading
in aggregate. We labeled it "tokens saved" without that caveat.

This release widens the dimension. Concretely:

- **New migration `0010_usage_rollups_per_repo.sql`** adds a
  `repo_fingerprint TEXT NOT NULL DEFAULT ''` column and changes the
  primary key to `(user_id, repo_fingerprint, day)`. Existing v0.3.2
  rows are migrated under `repo_fingerprint = ''` (the legacy bucket)
  so no data is lost; the table-rebuild dance is wrapped in the
  migration's implicit transaction.
- **`POST /v1/account/savings`** accepts an optional
  `repo_fingerprint` field — same 64-char SHA-256 hex format as the
  fingerprint that `recon init` already registers via
  `/v1/account/repos`. Older CLIs that omit it land in the legacy
  bucket; new CLIs pin to their real repo. A new `validOptionalFingerprint`
  guard rejects anything that isn't `''` / null / a 64-char lowercase
  hex string so we don't store arbitrary client text in a PK column.
- **`GET /v1/dashboard/savings`** now folds with
  `SUM(...) ... GROUP BY day` across `repo_fingerprint`. Single-repo
  users see byte-identical responses to v0.3.2 (one bucket per day);
  multi-repo users now see the true cross-repo total. Response shape is
  unchanged — clients that read `daily[*].tokens_saved` keep working.
- **`recon savings push`** computes the same canonical-path SHA-256
  fingerprint that `recon init` uses (via
  `recon_server::account::fingerprint_path`) and includes it in the
  body. `skip_serializing_if "".is_empty()` keeps the wire compact when
  fingerprinting is unavailable (e.g. a path that fails to canonicalise).

Backwards compat: a v0.3.2 CLI continues to push successfully against
the v0.3.3 worker and lands in the legacy bucket. A v0.3.3 CLI pushing
against a hypothetical v0.3.2 worker (which won't see this code, but
the field would be ignored) lands fine too — the worker's old PK would
just collapse those pushes into one row, identical to current behavior.

### Changed — savings panel chart upgrade

The dashboard's daily-savings chart was a 120 px-tall straight-line
polyline. Replaced with a 200 px chart that renders inline SVG only —
no chart library, no JS interactivity, no extra requests:

- Smooth curve via Catmull-Rom → Bézier conversion at tension 0.5;
  faithful at endpoints, no overshoot.
- Gradient-filled area below the curve from 32 % opacity at the line
  to 0 at the chart floor.
- Three dashed gridlines at 25 / 50 / 75 % of plot height.
- Per-point dots so the eye can count the sample size.
- Peak-day marker (filled dot + "Xk peak" label, edge-aware so it
  stays inside the canvas).
- Start/end date labels in muted mono on the bottom strip.
- Y-axis floor pinned to zero (not the data minimum) so a
  flat-but-nonzero series doesn't visually exaggerate.

Per-render unique gradient `id` so two charts on the same page never
collide. Rendering cost is unchanged (one DOM string-concat).

### Changed — savings labels are honest about being an estimate

Headline + sparkline + table copy across `site/dashboard/index.html`
and `site/js/dashboard.js`:

- "tokens · saved" → "tokens · estimated saved"
- table column "Saved" → "Est. saved"
- explainer paragraph rewritten to spell out: "Each tool call accumulates
  a hardcoded per-tool baseline (what the same question would have cost
  via Read+Grep+Glob) and an exact count of bytes recon emitted; saved
  = baseline − response. The baseline is a fixed estimate per tool,
  not measured against your repo, so this figure is a directional
  indicator, not a precise per-call measurement."

The previous wording implied a measurement; this is a transparent
model. Calling it that lets us keep showing the number without
overclaiming what it represents.

### Fixed — `recon savings` couldn't find the local DB

`recon savings push` and `recon savings show` looked for `.recon/recon.db`,
but `recon init` / `recon serve` / `recon doctor` all write `.recon/index.db`.
Every push immediately after `recon init` failed with the misleading
"run `recon init` or `recon serve` here first" message. Aligned the resolver,
doc comments, and error message in `crates/recon-cli/src/savings.rs` on
`index.db`. The doc strings in `main.rs` and `bench-real.rs` follow.
No reindex required after rebuild.

### Fixed — 8 of 20 MCP tools were unreachable via `recon query`

`server.rs::dispatch_tool` (the hand-written switch table that backs the
`recon query <tool>` CLI shim) listed only 12 of the 20 `#[tool(...)]`
exports. `code_path`, `code_callers`, `code_callees`, `code_context`,
`code_impact`, `code_subsystems`, `code_subsystem`, and `code_savings`
returned `unknown tool`. They worked over the rmcp `tool_router` (so
Claude Code / Cursor / Windsurf / opencode were unaffected); only the
CLI debug surface was missing them. All 20 are now wired.

### Fixed — `recon search` exact-mode lexical hits reported `line: 0`

The `code_search` exact-mode path emitted Tantivy hits without a line
number because `StructuredHit` carries `symbol_id` but no line. The
hardcoded `0` made every exact-mode result indistinguishable from
"line unknown". Now batch-resolved through
`ReadPool::symbol_locations_by_ids` (one query per response, not per hit)
and propagated. Regex and hybrid modes were already correct.

### Fixed — `recon strings -k literal` and `-k comment` returned identical results

The `kind` parameter on `code_find_strings` was echoed in the response
but never used to filter — `-k literal "API key"` returned `///` doc
comments, `-k comment` returned them too, and both matched `-k both`.
Added `classify_string_hit` (line-local heuristic over comment markers
and quote balance) and applied it as a hit-level filter; the response
`kind` field now reports the actual classification, not the requested
one. Edge cases acknowledged in the function doc: multi-line `/* */`
blocks and escape sequences inside strings can still misclassify; this
is a heuristic, not a lexer.

### Fixed — `recon multi` always emitted raw JSON

`recon multi 'fn x' impl` printed pretty-printed JSON regardless of the
`--json` flag, and the response shape diverged: a single pattern
produced `{hits, pattern}`, multiple patterns produced
`[{hits, pattern}, ...]`. Added `print_multi_group` to `pretty.rs` so
the default form is human-readable per pattern; `--json` keeps the
array shape for scripting.

### Fixed — `--json` silently ignored on four CLI handlers

`recon version --json`, `recon license --json`, `recon repos list --json`,
and `recon stats --json` all emitted the human-readable text branch
regardless. Each handler now branches on `cli.json` and emits a single
parseable JSON document. `stats --json` no longer appends the
`Indexed repos (global): N` trailer so consumers can pipe into `jq`
without filtering.

### Fixed — pre-existing FK panic in `storage_bench`

`crates/recon-storage/benches/storage_bench.rs::setup_store` inserted only
`src/lib.rs` while `make_symbol` referenced `src/file_*.rs`. The bench
panicked on the first `find_symbols_exact` iteration with
`FOREIGN KEY constraint failed`. Routed every `setup_store` caller
through `setup_store_multi_file` (which seeds one file row per
distinct path).

### Performance — watcher → query loop

End-to-end fix for the cold-cache stall on every save. Measured on
linux x86_64 (kernel 6.17.0-22) — storage benches via
`cargo bench -p recon-storage`, watcher loop via the new `bench-watcher`
bin (50 iterations + 50-file burst).

| Path | Before | After |
|---|---|---|
| `delete_cascade_loop/100_files` | **76.0 ms** (100 BEGIN/COMMIT) | one transaction |
| Watcher save → `code_outline` (p99) | **~350 ms** cold cache reload* | **0.46 ms** |
| Watcher save → `code_outline` (p50) | dominated by reload | **0.24 ms** |
| 50-file burst → first `code_find_symbol` lands | n/a | **312 ms** end-to-end (250 ms debounce + 60 ms parse/store) |
| `all_symbols/80k_across_1780_files` | 161.9 ms | now off the read path (background) |
| `all_refs/300k_across_1780_files` | 185.9 ms | now off the read path (background) |

\* The "before" figure is the storage-bench cold-cache cost
   (`all_symbols + all_refs ≈ 348 ms`) — the synchronous load that the
   read path used to pay on every save. There is no paired pre-change
   `bench-watcher` number because the harness was added in this same
   commit; the storage-bench numbers are the proxy.

Full numbers in `docs/PERF_BASELINE.md`.

Three changes, each defensible on its own:

- **`Store::delete_files_cascade(&[&Path])`** — one transaction with
  prepared statements amortized across the whole slice. The old
  per-file `delete_file_cascade` now delegates. `index_diff` and the
  watcher delete branch swap from the per-file loop. ~10–50× faster on
  branch switches and mass-delete refactors; same correctness.
- **Watcher Phase 2 parses in parallel.** The save-batch parse loop
  in `start_watcher` switched from `to_parse.iter().filter_map(...)`
  to `par_iter`, and `LanguagePools` is sized to
  `rayon::current_num_threads().max(4)` to match the rest of the
  indexer. Multi-file save bursts (rebase, format-on-save, mass touch)
  parallelize across cores instead of single-threading.
- **Async cache refresh — reads serve previous snapshot.** Watcher
  batches no longer clear `cached_paths` / `cached_symbols` /
  `cached_refs` synchronously. The next read tool used to pay
  `~350 ms` of cold `all_symbols` + `all_refs`; now an edge-triggered,
  coalesced background worker (`kick_async_refresh`) re-snapshots on a
  separate thread and arc-swap-in atomically. Reads see briefly-stale
  but warm caches during the refresh window — strictly better than
  the empty-cache cold reload. Coalescing pattern caps concurrent
  refresh threads at one even under rapid save bursts; a kick that
  arrives mid-snapshot retriggers exactly one extra iteration.

### Worker — production D1 was missing migration 0009

`GET /v1/dashboard/savings` was 500-ing for every dashboard load
because `usage_rollups` had never been applied to `recon-production`
on the Cloudflare side, even though `0009_usage_rollups.sql` shipped
with v0.3.2. Applied via
`wrangler d1 migrations apply recon-production --remote`. The table,
its primary-key auto-index, and the `(day)` covering index are now
live. The savings tab on the dashboard renders again — Free tier sees
the upsell card, Pro/Team see the empty-state placeholder until the
first push lands.

### Tests

- New `delete_files_cascade_multi_file` unit test in
  `recon-storage::store::tests` — covers multi-file delete +
  empty-slice no-op.
- New `bench_delete_cascade_loop` and `bench_delete_cascade_batched`
  criterion benches at sizes 100 and 500, plus the FK fix on the
  pre-existing `setup_store` helper.
- New `bench-watcher` binary
  (`crates/recon-cli/src/bin/bench-watcher.rs`) for save→query latency
  and 50-file burst measurement. Useful for verifying the async-refresh
  Phase 3 win on a real workload.
- 4 new worker tests in `worker/tests/savings.test.ts` covering
  per-repo SUM aggregation, legacy-bucket back-compat, mixed
  legacy + per-repo on the same user, and malformed-fingerprint
  rejection. 18 total in that file.
- 3 new CLI tests in `crates/recon-cli/src/savings.rs::tests` covering
  fingerprint pass-through, `skip_serializing_if` empty-string
  omission, and emission when present. 8 total.
- All 491 Rust workspace tests + 18 worker savings tests pass.

### Migration notes

- **Worker / D1**: migration `0010_usage_rollups_per_repo.sql` must be
  applied to production D1 (`wrangler d1 migrations apply
  recon-production --remote`). Idempotent on the existing dev/test
  setup via the migration runner pattern in `worker/tests/setup.ts`.
  The table-rebuild dance preserves all v0.3.2 rows under the legacy
  `''` repo_fingerprint bucket — no data lost.
- **CLI**: rebuild + reinstall (`cargo install --path crates/recon-cli
  --force` with the production HMAC key in env) to pick up the savings
  fingerprint, the `index.db` filename fix, the dispatch-table tools,
  and the search/strings/multi/--json fixes. No re-init required.
- **Single-repo users** see byte-identical responses on the dashboard
  pull endpoint vs v0.3.2 (one bucket per day; SUM of one row is the
  same number). Multi-repo users see a one-time correction in
  the headline and sparkline as the totals start summing across repos.

[0.3.3]: https://github.com/bravo1goingdark/recon/releases/tag/v0.3.3

## [0.3.2] — 2026-04-29

The savings dashboard. Pro/Team only.

### Fixed — server lifecycle on revocation

Before this release, `recon serve` did **not** shut itself down when
the worker rejected its license (account deletion, key revoke,
subscription hard-expiry) or when the user ran `recon logout` against
a running session. The periodic re-validation task would mark the
in-memory license `revoked = true` and wipe the credentials file, but
the process kept running indefinitely:

- **stdio**: held the IDE's stdio pipes open; every tool call
  returned `LicenseExpired` with no clear "this server is dead" signal.
- **HTTP** (`recon serve --port`): kept the listener bound on the
  configured port even though no agent could authenticate against it.
- watcher task, SQLite writer mutex, and telemetry counters all kept
  accumulating useless work.

`recon serve` now exits cleanly on any of:
- worker returning `Rejected` from `/v1/license/validate` (account
  deletion, key revoke, sub hard-expiry);
- credentials transitioning Some→None mid-run (the user ran
  `recon logout` against another shell);
- SIGINT / SIGTERM (unchanged behaviour);
- the IDE closing the stdio transport (unchanged behaviour).

**Implementation:** new `tokio::sync::Notify` field on `ReconServer`
plus two new methods, `request_shutdown()` (sets the flag and wakes
waiters) and `await_shutdown_request()` (the consumer side, used by
the serve `select!`). The periodic revalidation task fires
`request_shutdown()` on Rejected; the stdio + HTTP serve loops add it
as a third `select!` arm alongside signals and transport-close.

Four new tests in `recon-server::server::tests` (request → await
round-trip, fast-path short-circuit when already requested, idempotency
of repeated requests, full `shutdown()` also wakes outstanding waiters).



The local `code_savings` counter from v0.3.1 stays available to every
user (it's just a query against `.recon/recon.db`), but team-level
visibility — "how much did engineering save in API tokens this
month?" — now flows through the worker into the recon dashboard at
`mcprecon.pages.dev/dashboard`.

### Added

- **`POST /v1/account/savings`** — license-key-authed push endpoint.
  Body: `{day, calls, response_tokens, baseline_tokens, tokens_saved,
  latency_micros}`. Pro/Team get 200; Free gets 402 with an upsell
  payload. Idempotent and **monotone**: the upsert is `MAX`-merged on
  `(user_id, day)`, so a stale CLI cannot regress the stored counter
  and re-runs cannot double-count. Day format strictly validated; all
  counters validated as non-negative safe-int.
- **`GET /v1/dashboard/savings`** — session-authed pull endpoint.
  Returns the daily series + aggregate totals. Range cap by tier:
  Free 0d (upsell payload, no D1 read), Pro 30d, Team 90d, Enterprise
  365d. Honours `?range=N` down-shift, clamped to the cap. Hot path is
  one equality+range scan on the `(user_id, day)` PK — index-only,
  no second round-trip for totals (folded JS-side over ≤365 rows).
- **`recon savings push`** — CLI subcommand. Reads the local
  telemetry counters from `.recon/recon.db` (`tel:tool:*` meta keys),
  aggregates today's snapshot, posts to the worker. Surfaces the
  Pro-only 402 with a clean upgrade message instead of a stack trace.
- **`recon savings show`** — local-only TSV print of the same numbers
  (no network), so you can sanity-check what's about to be pushed.
- **Dashboard "Savings" tab** — fifth tab on the account dashboard.
  Big tokens-saved headline, inline-SVG sparkline of the daily series
  (no chart library), per-day TSV table. Free tier sees an upgrade
  card with a link to `/pricing`.
- **Pricing page** — Pro/Team cards now list the savings-dashboard
  retention as a feature line.

### Database

- New migration **`0009_usage_rollups.sql`** — table with composite PK
  `(user_id, day)`, plus a covering `(day)` index for the future cron
  compaction job ("delete rollups older than 90 days"). Migration runs
  forward only; existing tests apply it via the `applyD1Migrations`
  pattern in `worker/tests/setup.ts`.

### Performance

- Pull endpoint: **one** D1 round-trip per dashboard load. The PK
  `(user_id, day)` makes the range filter `WHERE user_id = ? AND day
  >= ? AND day <= ?` a contiguous B-tree slice; `day ASC` ordering is
  already in PK order so no extra sort. Aggregation runs in JS over
  the ≤365 returned rows — measurably faster than a second SUM()
  round-trip across the network.
- Push endpoint: single-statement `INSERT … ON CONFLICT … DO UPDATE
  SET col = MAX(existing.col, excluded.col)` — atomic, idempotent,
  monotone in one SQLite transaction. No application-level locking.

### Privacy

- The push payload is six integers per day per user. **No code, no
  symbol names, no file paths, no query strings travel.** Same weight
  as a SaaS reporting "you made N API calls today." The privacy
  paragraph on the Docs telemetry section spells this out.
- Free tier never pushes, never has rows in `usage_rollups`. The
  table only accumulates for paying accounts.

### Tests

- 14 worker tests in `worker/tests/savings.test.ts` covering: Pro/Team
  push acceptance, Free 402 + upsell shape, MAX-merge monotonicity,
  fresh-write upsert, day-format validation, counter validation
  (negative + non-integer rejection), 401 without auth, range cap by
  tier, range down-shift + clamp, scope isolation across users.
- 5 CLI unit tests in `recon-cli/src/savings.rs::tests` covering
  the civil-from-days date math, today_utc shape, aggregation across
  per-tool counters, savings clamp at zero, empty input.

[0.3.2]: https://github.com/bravo1goingdark/recon/releases/tag/v0.3.2

## [0.3.1] — 2026-04-28

The two blockers between v0.3.0 and "I'll buy this" closed in one release:
**multi-language parser parity** and **token-savings telemetry**.

### Added — multi-language parity

All five non-Rust extractors now walk function/method/class/struct bodies
and emit identifier refs from their identifier arms. The reference graph
is now meaningfully populated for every supported language; `code_path`,
`code_callers`, `code_callees`, `code_context`, `code_impact`,
`code_subsystems`, and `code_repo_map` work cross-language.

- **Python** (`extract_python`): `function_definition` and
  `class_definition` now walk their full bodies for identifier refs;
  `decorated_definition` attributes the decorator's identifier as a ref
  from the decorated symbol; class bases (e.g. `class Derived(Base):`)
  produce refs to base classes.
- **JavaScript / TypeScript / TSX** (`extract_js_ts`): `function_declaration`,
  `method_definition`, `class_declaration`, `interface_declaration`,
  `enum_declaration`, `type_alias_declaration`, plus arrow-function and
  function-expression values inside `lexical_declaration` /
  `variable_declaration` all walk their bodies. Identifier arm covers
  `identifier`, `property_identifier`, `type_identifier`,
  `shorthand_property_identifier`. `extends Base implements Iface` now
  produces refs.
- **Go** (`extract_go`): `function_declaration`, `method_declaration`,
  and `type_spec` (struct / interface bodies) all walk for identifier
  refs. Method receivers are now reported as refs from the method.
- **Java** (`extract_java`): `method_declaration`, `constructor_declaration`,
  `class_declaration`, `interface_declaration`, `enum_declaration` all
  walk their bodies. Generics (`List<String>`) produce refs to all type
  identifiers.
- **C / C++** (`extract_c_cpp`): `function_definition` walks its body;
  `struct_specifier` / `class_specifier` / `enum_specifier` walk their
  full nodes for type-ref collection. Template arguments
  (`std::vector<MyType>`) are captured.

Per-language regression tests in `crates/recon-parser/src/extract.rs::tests`
assert non-empty refs for each language on representative fixtures
(`python_refs_extracted_from_function_body`,
`python_refs_extracted_from_class_bases`,
`typescript_refs_extracted_from_function_and_class`,
`javascript_refs_extracted_from_function_body`,
`go_refs_extracted_from_function_body`,
`java_refs_extracted_from_method_body`,
`cpp_refs_extracted_from_function_body`).

### Added — token-savings telemetry

A new `crates/recon-server/src/telemetry.rs` module tracks, per registered
tool: call count, response token estimates, baseline tokens avoided
(what Read+Grep would have cost), and per-handler latency. Atomics on the
hot path; SQLite-backed lifetime persistence.

- **`code_savings` tool** — returns a tab-separated breakdown of every
  tool's calls / response tokens / baseline tokens / tokens saved /
  average latency, followed by an aggregate trailer. Output uses
  `Skeleton`.
- **`code_stats`** now includes a `telemetry` block with session
  uptime, total calls, response_tokens, baseline_tokens_avoided, and
  tokens_saved. Backward compatible — added as a new top-level field;
  existing fields unchanged.
- **Persistence** — every `FLUSH_THRESHOLD` (default 50) tool calls,
  the server spawns a `tokio::task::spawn_blocking` to write per-tool
  counters to the SQLite `meta` table under `tel:tool:<name>` keys.
  `ReconServer::shutdown` performs a synchronous flush so the trailing
  window is captured before exit. Hydration on startup merges the
  persisted lifetime counters into freshly-initialised atomics.
- **Per-tool baselines** — conservative, audit-friendly token-cost
  estimates documented in `BASELINES` with one-line rationales (e.g.
  `code_repo_map: 20000 tokens — Read 5 files for orientation`).
  Static constants by design.
- **Model-agnostic by design** — recon reports *tokens* saved, not
  dollars. Agents calling these tools may run on Claude, GPT, Gemini,
  a self-hosted Llama, or anything else, each with its own pricing
  and discount structure. Hard-coding a "$X saved" figure would
  privilege one provider's list price; we leave the conversion to
  the caller's actual rate sheet.
- **Hot-path overhead** — each tool call adds 4 atomic adds + one
  `tiktoken-rs` `estimate_tokens` pass over the response (≈ 250 µs for
  a 2 KB response). The threshold-driven flush is async; the only
  synchronous SQLite write is at shutdown. Worst-case telemetry cost
  is bounded by the response size, never by tool latency.

### Changed

- **`ReconServer::new`** now takes `&store` to hydrate telemetry from
  the meta table before the store is moved into the Mutex. No public
  signature change.
- **Watcher cache invalidation** also clears `cached_call_graph` along
  with `cached_symbols` / `cached_refs` so graph tools rebuild against
  fresh data after every save.

### Performance

- Telemetry record path is lock-free except on flush; flush is
  async-spawned and protected by a mutex inside `Telemetry` so
  concurrent flushes serialize without blocking the hot path.
- Per-tool snapshot reads (used by `code_savings` and `code_stats`)
  perform exactly one `Acquire` load per atomic, no allocation.

### Tests

- 7 new telemetry unit tests in `recon-server::telemetry::tests`
  (baseline lookup, threshold trigger, hydrate round-trip,
  saturating-subtraction, unknown-tool handling).
- 12 new handler tests in `recon-server::server::tests` covering all
  Phase-1+2 graph tools, `code_savings`, the `code_stats` telemetry
  block, and a server-restart persistence round-trip.
- 7 new per-language ref-extraction tests in
  `recon-parser::extract::tests`.
- Tool-description audit (`tool_descriptions_under_2kb`) extended with
  `code_savings` (under 1 KB).

### Risk register notes

- Telemetry baselines are static. When new tools land, update
  `BASELINES` first or the baseline lookup returns 0 (silent
  zero-savings rather than a panic).
- `ReconServer::shutdown` was previously safe to call multiple times;
  it remains so. The synchronous telemetry flush takes the
  `flush_guard` mutex, so a second concurrent shutdown serializes
  cleanly.

[0.3.1]: https://github.com/bravo1goingdark/recon/releases/tag/v0.3.1

## [0.3.0] — 2026-04-28

Graph-traversal MCP tools, end-to-end. Cuts the canonical
`find_symbol → read_symbol → find_refs → search-for-tests` agent loop down
to a single tool call; replaces chained `code_find_refs` walks with one
n-hop traversal. Inspired by `graphify`, `codegraph`, and Anthropic's
"MCP code execution" pattern — concrete recon shape: forward + reverse
CSR over the existing `refs` table, lazy-built and cached alongside
`cached_symbols` / `cached_refs`.

### Added

- **`code_path src dst [max_hops=8]`** — bidirectional BFS shortest path
  between two symbols. Returns an ordered hop sequence with file:line per
  hop. Reports `unresolved_hint` when the BFS terminates near a
  dyn-dispatch / FFI boundary. Output uses ReferenceDigest with the new
  `path` field.
- **`code_callers symbol [depth=1]`** / **`code_callees [depth=1]`** —
  layered transitive traversal up to `depth` rings (max 6). Cycle-safe;
  per-tier fan-out cap 50; total-visit cap 50 000. Output uses
  ReferenceDigest with the new `tiers` field.
- **`code_context symbol_or_query`** — one-shot bundle replacing the
  4-call understand-X loop: target skeleton + body + up to 5 callers / 5
  callees / 3 types / 3 tests, honoring `token_budget` (default 2000)
  with priority-ordered drop. Output uses SymbolCard with the new
  `context` envelope.
- **`code_impact symbol [depth=4]`** — blast radius. Tiered transitive
  callers + transitively-reachable test functions. Test detection in
  v0.3 is Rust-only (`tests::*` qnames + `test_*` / `Test*` heuristic);
  cross-language detector is Phase 2 v0.4.x.
- **`code_subsystems`** / **`code_subsystem <id>`** — repo orientation
  via weakly-connected components of the reference graph. Each subsystem
  reports its hub (highest-degree symbol), dominant directory, and
  member count. v0.3 uses union-find connected components; v0.4.x will
  upgrade to Leiden modularity-optimized clustering.
- **Centrality in `code_stats`** — adds `top_in_degree` and
  `top_out_degree` arrays (top-20 each, top-level symbols only).
  PageRank/betweenness centrality columns deferred to v0.4.x with the
  schema migration.
- **New error variant `ReconErrorCode::ResourceExhausted`** for
  graph-budget-exhausted paths (visit cap hit, fan-out cap hit). Stable
  numeric code -32012, kebab-case kind `resource_exhausted`.

### Fixed

- **Parser now extracts call/use refs from inside Rust function bodies,
  struct field types, and enum variant payloads.** Previously
  `extract_rust` only recursed into module bodies and trait/impl
  bodies — function bodies were skipped, so the reference graph had no
  call edges at all. PageRank still produced reasonable rankings off
  module-level refs, but `code_path`, `code_callers`, etc. would have
  found empty graphs without this fix. (`crates/recon-parser/src/extract.rs`)
- **Storage now remaps parser-local symbol ids to DB rowids on insert.**
  Each file's parser starts `next_id` at 1; SQLite auto-assigns rowids
  continuing from `MAX(id)`. `Ref::src_symbol_id` and `Symbol::parent_id`
  carry parser-local ids, so without remap, every file after the first
  has its refs and parent pointers point at wrong global symbols.
  Affected: `batch_index_file` and `batch_index_files`. Existing single-
  file tests passed because the first file's local ids happen to match
  DB rowids when the table is empty.
  (`crates/recon-storage/src/store.rs`)

### Changed

- **`RefDigestView`** gained optional `path`, `tiers`, `truncated`,
  `unresolved_hint`, `tests` fields — all `skip_serializing_if` so
  `code_find_refs` responses are byte-identical to v0.2.x.
- **`SymbolCardView`** gained an optional `context` envelope —
  `code_read_symbol` responses are byte-identical when no envelope is
  attached.
- **Server**: new `cached_call_graph: Arc<ArcSwapOption<CallGraph>>`
  built lazily on first graph-tool call after each cache invalidation.
  Watcher invalidation paths clear it alongside symbols/refs.

### Performance

- BFS over the cached forward+reverse CSR. `code_path` typical < 5 ms;
  worst-case bounded by total-visited cap (50 000 nodes).
- Connected components via path-compressed union-find — `O((V+E) α(V))`.
- All tools share a single `CallGraph` instance per cache generation;
  graph build is `O(V+E)` (~30–50 ms on a 500K-symbol repo) but amortizes
  to O(1) per query after the first call.

### Tests

- 16 unit tests in `recon-search::graph` (path/callers/callees/cycle/
  components/degree/per-tier-cap/visit-cap).
- 6 new shape serde tests in `recon-core::shapes` (legacy shape byte-
  compat + new mode shapes).
- 17 new handler tests in `recon-server` covering all 7 new tools and
  the new `code_stats` centrality fields.
- Tool-description audit (`tool_descriptions_under_2kb`) extended with
  the 7 new descriptions; longest is `code_context` at 1.4 KB.

[0.3.0]: https://github.com/bravo1goingdark/recon/releases/tag/v0.3.0

## [0.2.4] — 2026-04-27

v0.2.4 supersedes v0.2.3 — same fixes, plus a CI-only test skip for the two
`watcher_delete.rs` integration tests that hung the macos-latest job. Real
macOS users are unaffected: the watcher mechanism is verified by the
`recon-indexer` unit tests (which all pass on macos-latest), and the cascade
end-to-end is verified on Linux + Windows. The integration tests assert the
SQLite/Tantivy/vector-store cascade completes within a 1.5 s settle window;
GitHub's virtualized macos-latest runner delivers FSEvents 5–30 s after the
syscall, the assert fires before the cascade, the test panics before
`server.shutdown().await`, and the orphan `spawn_blocking` watcher task
prevents the test binary from exiting. Marked `#[ignore]` on macOS with a
TODO to re-enable in 0.2.5 with a poll-assert + Drop-guard.

### Fixed

- **macOS release pipeline still hung after the v0.2.3 `recv_timeout` fix.**
  The fix to `watcher_recv_blocks_until_event` was correct but not
  sufficient — two integration tests in `crates/recon-cli/tests/watcher_delete.rs`
  hit the same FSEvents-latency / panic-skips-shutdown / orphan-blocking-task
  pattern. `#[cfg_attr(target_os = "macos", ignore = "...")]` on both
  pending the proper poll-assert + Drop-guard rewrite.

[0.2.4]: https://github.com/bravo1goingdark/recon/releases/tag/v0.2.4

## [0.2.3] — 2026-04-27 — superseded by 0.2.4

> The 0.2.3 tag was pushed but its release pipeline failed: two integration
> tests in `watcher_delete.rs` hung the macos-latest job, the workflow's
> new `timeout-minutes: 30` guard fired (so it didn't burn 6 h of CI
> minutes — that part of v0.2.3 worked), but the build job is gated on
> all test legs and never started. No binaries shipped under v0.2.3.
> All v0.2.3 fixes are also in v0.2.4. Skip ahead to 0.2.4.

v0.2.3 supersedes v0.2.2: the v0.2.2 tag was pushed but its release pipeline
hung indefinitely on the `macos-latest` test job (no `timeout-minutes` set,
FSEvents in CI failed to deliver an event under a `RecursiveMode::NonRecursive`
root watch and a `recv()` call with no timeout sat forever). v0.2.2 never
produced binaries — anyone who runs `recon update` will receive v0.2.3.

### Fixed

- **`code_outline` dropped methods inside `impl` blocks.** The Rust extractor
  in `crates/recon-parser/src/extract.rs` parented `impl_item` methods to a
  `Some(0)` sentinel instead of looking up the struct/enum/trait id. The
  outline filter (`parent_id.is_none()`) silently excluded them, and
  `code_read_symbol` parent chains skipped the type. The parser now resolves
  the `impl` target's id (with generics stripped: `Foo<T>` → `Foo`) and
  threads it through; the server-side outline also rescues legacy `Some(0)`
  rows by parsing `qualified_name` "Type::method" prefixes against the
  in-file type map, so the fix takes effect without forcing a reindex.
- **`code_skeleton` lost doc comments above attributed items.** `leading_doc`
  walked previous siblings backward and broke on anything that wasn't a
  comment or expression statement — so `#[derive(...)]` / `#[inline]` /
  `#[repr(...)]` / Python `@decorator` between the doc and the item
  terminated the walk before reaching the doc. The walk now skips
  `attribute_item` / `inner_attribute_item` / `decorator` siblings.
- **`code_find_refs` digest filled with degenerate `{path:"", line:0}` rows.**
  When a ref's `src_symbol_id` had no matching location row (orphan from a
  pre-watcher-fix deletion), the digest emitted an empty path before the
  top-20 cap, polluting the output. Filter is now applied *before* the cap
  and `total` reports the count of valid (locatable) refs.
- **`code_repo_map` over-ranked `#[cfg(test)] mod tests` content.** Test
  callers at single out-edge nodes propagated full PR weight into the
  production hubs they exercised, and the `tests` module itself appeared
  high in repo orientation. Refs originating from any test scope (qualified
  name `tests`, `tests::*`, `*::tests::*`, `*::tests`) are now skipped at
  graph-build time so they don't inflate target scores; symbols inside
  test scopes also have their final score multiplied by 0.1 so the `tests`
  module drops below real production hubs in the rendered map.
- **macOS release pipeline hung indefinitely on `cargo test`.** The
  `watcher_recv_blocks_until_event` test in `crates/recon-indexer/src/watcher.rs`
  called `Watcher::recv()` (blocking, no timeout). Under v0.2.2's new
  `RecursiveMode::NonRecursive` root watch, FSEvents in the macOS-latest
  CI runner did not deliver the `delayed.rs` create event reliably and the
  test wedged forever — held the runner for 1h+ until manually cancelled.
  Replaced with `recv_timeout(Duration::from_secs(10))`, and added
  `timeout-minutes: 30` to the `test:` step in both `release.yml` and
  `cross-platform.yml` so a future regression of this shape fails fast
  instead of consuming a 6 h job slot.

### Migration notes

This is a patch release — no schema or config changes. The `code_outline`
fix takes effect without a reindex (server-side rescue path handles legacy
rows); `code_skeleton` doc rendering improves the next time a file with
attributed items is touched (or after `code_reindex --force`).

[0.2.3]: https://github.com/bravo1goingdark/recon/releases/tag/v0.2.3

## [0.2.2] — 2026-04-27 — superseded by 0.2.3

> The 0.2.2 tag exists but no binaries were ever published — the
> macos-latest test job in the release pipeline hung indefinitely and was
> cancelled. All fixes listed here are also in 0.2.3, which additionally
> resolves the macOS hang itself. Skip ahead to 0.2.3.

### Fixed

- **Watcher silently dropped delete and rename events.** The
  notify-debouncer filter at `crates/recon-indexer/src/watcher.rs` checked
  `p.is_file()`, which returns `false` for paths that no longer exist —
  so deletion events never reached the indexer. Symbols from removed
  files lingered in SQLite, Tantivy, and the embedding store until the
  user manually ran `code_reindex --force`. Replaced with `!p.is_dir()`
  (excludes directories, keeps deleted-file events) and added a
  Phase 0 in `start_watcher` that snapshots symbol IDs, then cascades
  through SQLite (`delete_file_cascade`), Tantivy (new `delete_path`),
  and the vector store (new `delete_by_symbol_ids`). Rename is the
  same shape — old path treated as delete, new path as create.
- **Watcher saturated by `cargo build` storms.** A single recursive
  watch on the repo root pulled every `target/` subdir into inotify
  (8.6k dirs in this workspace). Build-time file activity overflowed
  the kernel's 16k inotify event queue → `IN_Q_OVERFLOW` → silent
  loss of legitimate source-file edits — the user would edit a file,
  query immediately, and see stale results. Replaced with a
  non-recursive watch on the root plus per-top-level-child recursive
  watches that exclude `target/`, `node_modules/`, `.git/`, `.recon/`,
  `.idea/`, `.vscode/`. Also broadened the overflow-fallback regex
  (`overflow` / `coalesced` / `lost` / `queue`) so more notify error
  phrasings reliably trigger the `gix status` recovery path.
- **`refresh_caches` was non-transactional.** Path / symbol / ref
  caches were populated from three independent SQLite read connections.
  A concurrent writer between any two reads left the caches reflecting
  different point-in-time states (e.g. symbols referencing a path no
  longer in the path list). New `ReadPool::snapshot_all_for_caches`
  wraps all three reads in one transaction on a single connection.
- **`recon init --mcp cc` no longer silently skips agent rules when
  `CLAUDE.md` is missing.** Previously the init flow saw "no
  `CLAUDE.md`", printed a one-line skip message, and returned success —
  Claude Code then started without recon's strict-policy block, the
  agent defaulted to `Read`/`Grep`/`Glob`, and the whole point of the
  recon `code_*` tooling was silently absent.  Init now creates
  `CLAUDE.md` when missing and writes the marker-fenced rules block in
  it (matches the behavior already in place for opencode's
  `AGENTS.md`).  Symmetric purge: `recon purge --mcp cc` deletes the
  file outright if its only content was the recon block, so we don't
  leak a file we created ourselves; user-authored content keeps the
  file alive with only the recon block stripped.
- **`smallvec` `write` feature missing from workspace.**
  `recon-search/src/pagerank.rs` uses `write!(line_buf, ...)` on a
  `SmallVec<[u8; 256]>`, which requires the `write` feature.
  Workspace `Cargo.toml` only declared `serde`. Workspace builds
  succeeded by accident because `gix-object` transitively enabled
  `smallvec/write` and Cargo's feature unification spread it to
  every crate — single-crate `cargo build -p recon-search --lib`
  failed with `cannot write into SmallVec<[u8; 256]>`. Now declared
  explicitly.

### Performance

- **Token diet for every tool response.** All canonical view types in
  `recon-core::shapes` now skip `None` and empty-`Vec` fields when
  serialising — `RefEntry::col`, `RefEntry::enclosing_symbol`,
  `SymbolCardView::signature`, `SymbolCardView::doc`,
  `SkeletonView::path`, `SymbolCardView::parent_chain` /
  `callers` / `callees`, and `OutlineEntry::children`. The ad-hoc text
  search hits in `code_search` (lexical, regex, Tantivy fallback)
  also stop emitting `"col":null` on every row. Previously every
  symbol card carried `"callers":[],"callees":[]` (~26 bytes) even
  when nothing was resolved, and every leaf in a `code_outline` carried
  `"children":[]` (~14 bytes per leaf). On a 50-symbol outline the
  combined savings round to ~700 bytes / ~175 tokens; on a dense
  `code_search` with 100 lexical hits, ~10–15 bytes per hit times
  the population.
- **`code_reindex --force` clears the index in O(1) transactions.**
  Was N transactions (one `delete_file_cascade` per file with a WAL
  fsync each), a multi-second hot spot on large repos. New
  `Store::delete_all_files_cascade()` does the truncation in one
  `BEGIN`/`COMMIT` — `DELETE FROM refs; DELETE FROM files;` and the
  schema cascade handles symbols → symbol_docs → FTS triggers.
- **Embed handles use lock-free `ArcSwapOption`.** `embed_service`
  and `vec_read_pool` were `Arc<Mutex<Option<Arc<…>>>>` — set once
  in `init_embed` but read on every embed-backed tool call (semantic
  search, semantic find-symbol, watcher embed batch). The
  `parking_lot::Mutex` reads are now lock-free `load_full()` calls.
- **`index_repo` releases locks around `incremental_vacuum`.** Both
  writer locks are now released between the indexing pass and VACUUM,
  so VACUUM only holds the SQLite writer. Cache pre-warm runs without
  any locks held.
- **Embed catch-up cleans up orphan embeddings.** When the watcher
  starts, embeddings whose underlying symbol is no longer in SQLite
  (legacy from pre-fix watchers, or out-of-band index wipes) are now
  removed alongside the missing-symbol embed pass. Added
  `VecReadPool::all_embed_ids()` for the diff against current symbol IDs.

### Migration notes

This is a patch release — no schema or config changes. Existing users
pick this up via `recon update`; no `recon login` or `recon init`
re-run is required. The watcher delete-fix is silent: the first time
the new binary starts, it cleans up any orphan embeddings left over
from deletes that happened under earlier versions, then runs as before.

**Wire-format note for third-party MCP clients.** The token-diet entry
in *Performance* changes the JSON shape: optional fields that used to
serialise as `null` (`col`, `enclosing_symbol`, `signature`, `doc`,
`path` on aggregated skeletons) and empty arrays (`callers`, `callees`,
`parent_chain`, `children`) are now **omitted** instead of emitted as
`null`/`[]`. LLM consumers (the canonical client) are unaffected — they
read content, not structure. Custom clients that pattern-match on field
presence (e.g. `if (hit.col === null)`, `response.callers.length`)
should treat **omitted optional fields as `null`** and **omitted list
fields as `[]`**. The recon binary itself was the only known parser of
this shape and has been updated.

[0.2.2]: https://github.com/bravo1goingdark/recon/releases/tag/v0.2.2

## [0.2.1] — 2026-04-25

### Fixed

- **`recon init --mcp <ide>` now smoke-tests the server before declaring
  success.** Previously, when `recon serve` failed at startup (rejected
  license, over-tier repo, panic during indexer init, missing
  credentials, …) the IDE surfaced only `MCP error -32000: connection
  closed` with no detail — the child process's stderr was swallowed by
  the MCP transport in Claude Code / opencode / Cursor / Windsurf and
  routed to a debug log most users never read. `init` now spawns the
  same binary it just wired into the IDE config, waits 4 s, and either
  declares the test passed (server stayed alive) or surfaces the
  child's stderr verbatim inside a clearly labeled block, with a hint
  that this is the same content the IDE would have hidden as
  `connection closed`. Idempotent — re-run `recon init --mcp <ide>`
  after fixing the surfaced cause.

[0.2.1]: https://github.com/bravo1goingdark/recon/releases/tag/v0.2.1

## [0.2.0] — 2026-04-25

### Added

- **Server-side repo enforcement.** `max_repos` is now enforced by the
  recon worker. `recon init` registers each repo's canonical-path
  SHA-256 fingerprint via atomic `POST /v1/account/repos` (single-statement
  `INSERT … SELECT … WHERE` so concurrent inits at limit-1 cannot both
  win). Replaces the prior local-file enforcement that a patched binary
  trivially bypassed.
- **`recon repos list / remove`** for managing slots from the CLI.
  `remove` accepts either a path or a 64-char fingerprint pasted from
  `list`. Best-effort cleans the local cache too.
- **`recon doctor [--json]`** — health check across binary, repo dir,
  global config dir, license cache, credentials file (mode 0600 on
  Unix), worker `/v1/health`, authenticated worker repo list, index
  state (read-only SQLite open — does not load `ReconServer`), MCP
  wiring across cc / oc / cursor, and agent rules across CLAUDE.md /
  AGENTS.md / cursor.mdc / windsurf.md. Exit 1 on any FAIL.
- **Worker:** new `requireApiKey` middleware, `RL_ACCOUNT` rate-limit
  binding (60 / min / key-prefix), `/v1/health` (and `/api/v1/health`)
  endpoint for the doctor to ping.
- **Resume-or-swap on `/v1/billing/subscribe`.** Re-clicking Subscribe
  after dismissing the modal now (a) resumes the same upstream
  subscription if Razorpay still has it in `created` and the user
  picked the same tier+currency — returns the original `subscription_id`
  with `resumed: true` and a fresh `short_url`; (b) cancels-and-
  recreates if the user switched tier/currency, or the upstream sub
  expired/404'd. The dashboard's Cancel button now also renders for
  `created`/`authenticated`/`pending` rows so users can break out of an
  abandoned attempt without contacting support.
- **Razorpay Checkout SDK redirect + dashboard auto-poll.** `/pricing`
  now opens Razorpay's hosted Checkout widget (instead of redirecting
  to the legacy short-URL page); on success the SDK redirects to
  `/dashboard?just_paid=1`, where the page polls `/v1/billing/portal`
  for ~30s until the `subscription.activated` webhook lands and the
  tier flips. No more "I paid, but the dashboard still says Free" UX.
- **Detailed plan descriptions on Razorpay Checkout.** Plans created
  via `ensurePlanForTier` now carry a tier-specific description
  ("Up to 10 repos · 5,000 files/repo · 200K LOC. Priority support…"),
  visible on the hosted checkout page and on the receipt email.
- **Dashboard: server-side registered repos.** New panel lists every
  repo registered via the worker (path + fingerprint + last-seen) with
  a Remove action that calls `DELETE /v1/account/repos/:fingerprint`
  and refreshes the list inline.
- **Site:** mobile hamburger menu across landing / docs / pricing /
  login / dashboard. Below 720 px (900 px on Docs) the wide nav `<ul>`
  is replaced by a burger button that opens a fixed-position sheet
  with every desktop link plus Sign-in / Sign-out as appropriate.
  Sheet closes on link tap, Escape, or breakpoint-up; body scroll is
  locked while open. Shared CSS in `/css/nav-menu.css`, JS in
  `/js/nav-menu.js` (CSP-compliant, `script-src 'self'`).
- **Site: manifesto-type OG card.** New 1200×630 `og.png` rendered
  from `scripts/og-banner.html` (Instrument Serif headline "35× fewer
  tokens. same answer." over the paper palette), plus a 1500×500
  X/Twitter header (`site/banner.html`). Existing meta tags pick the
  new OG image up automatically.
- **Site: brand logo PNGs.** `site/logo.png` (1024×1024 transparent)
  and `site/logo-512.png` (512×512 paper-bg) sourced from
  `scripts/logo.html` for use as a GitHub org avatar / Razorpay
  merchant logo / general brand asset.
- **Docs:** new `Account & repos` and `Troubleshooting` sections in
  `site/Docs.html` covering server-side enforcement, slot management,
  common failure modes, and `recon doctor` output.
- **Site: copy-to-clipboard on every CLI snippet.** A small button on
  every `<pre>` copies the command without the comments / output, so
  paste-from-docs works without re-editing.
- **Docs sidebar: collapsible groups.** Long sidebar groups
  (`Accounts`, `Project`, `Server commands`, `Direct query`) collapse
  into `<details>` so users can hide the sections they don't need.

### Changed

- **`recon purge --mcp <ide>`** now also calls
  `DELETE /v1/account/repos/:fingerprint` to release the server-side
  slot. Best-effort; idempotent for pre-v0.2 repos that were never
  registered.
- **`recon init` requires credentials.** v0.2 needs the raw API key
  for the registration POST, not just the cached signed license. Users
  who upgraded from v0.1 may need to run `recon login <key>` once to
  regenerate the credentials file.
- **Parser unit tests.** Added `tsx_basic` and `javascript_basic`
  covering the two of nine indexed languages that previously had only
  transitive coverage via the multi-language e2e test.
- **Homepage rewrite.** New IDE matrix (Claude Code / opencode / Cursor
  / Windsurf), per-OS install picker, real Free-tier limits surfaced
  inline, and a 4×1 vertical install grid that doesn't horizontally
  scroll on mobile.
- **`Docs.html` rewrite around CLI usage.** Replaces the previous
  internals-heavy structure (output shapes, search tiers, ADRs) with
  a CLI reference grouped by use case (Accounts / Project / Server /
  Direct query) — synopsis + description + worked example for every
  one of recon's 24 commands.
- **Razorpay HTTP layer.** Calls now retry 2× with exponential backoff
  on 5xx + network errors, 10 s `AbortController` timeout, typed
  `RazorpayHttpError` so retry logic branches on `.status` rather than
  matching error strings.
- **OS tabs on `/install`** are now CSP-safe (no inline `onclick`);
  every tab is bound through `addEventListener` in `os-tabs.js`.

### Fixed

- **Three critical billing races + replay windows on `/subscribe`.**
  - **Race-free placeholder INSERT.** The old `SELECT-existing →
    Razorpay → INSERT` shape let concurrent clicks all pass the
    SELECT, double-charge upstream, and double-INSERT. Replaced with
    a single atomic `INSERT … WHERE NOT EXISTS` that claims the slot
    *before* the Razorpay call; losing requests get 409 without
    touching Razorpay. `notes.placeholder_id` lets the webhook
    self-heal if our post-Razorpay UPDATE fails.
  - **Status-guard on `subscription.charged`.** A delayed `charged`
    arriving after `cancelled`/`completed`/`expired` no longer
    resurrects the sub. Out-of-order or replayed webhooks are
    recorded in `webhook_events_dropped` and skipped.
  - **NULL `current_end` refused.** Subscription events without a
    `current_end` are dropped (granting tier with `expires_at = NULL`
    would write a never-expiring api_key — the cron skips NULL by
    design — trapping users in permanent free Pro).
  - 24 h replay-window guard on `event.created_at` (matches Razorpay's
    retry envelope) and event-id idempotency keyed on
    `X-Razorpay-Event-Id` (migrations 0006 + 0007).
- **Swap-path race that orphaned in-flight Razorpay subs.** The
  resume-or-swap branch unconditionally `DELETE`d an existing
  `created` placeholder — even when its `razorpay_subscription_id`
  was still `NULL` (a concurrent `/subscribe` was mid-`createSubscription`).
  Deleting it let the next request claim a fresh slot and call
  Razorpay again, double-billing upstream. The swap branch now
  returns 409 when `razorpay_subscription_id IS NULL` and lets the
  in-flight request finish.
- **Razorpay checkout iframe permissions.** `accelerometer` and
  `gyroscope` are now explicitly delegated to `checkout.razorpay.com`
  + `api.razorpay.com` in the site `Permissions-Policy`, so Razorpay's
  fraud-risk fingerprinting on mandate authorisation no longer logs
  "blocked by permissions policy".
- **Site CSP allows Google Fonts.** `style-src` now lists
  `https://fonts.googleapis.com` and `font-src` lists
  `https://fonts.gstatic.com`. The previous CSP silently blocked the
  font stylesheet and `.woff2` binaries on every browser that
  enforced CSP, so the site fell back to system fonts (Times /
  Helvetica / Courier) since v0.1.0.
- **Pricing/Free tier link** no longer triggers a 400 alert; the
  footer copy reflects the active currency rather than hard-coding USD.
- **Site horizontal scroll** killed across narrow viewports —
  oversize `<pre>` blocks scroll within their container instead of
  pushing the page wider than the viewport.
- **`recon init` on unsupported platforms** drops the public-repo
  URL from the error string (was a leftover from the open-core era).

### Migration notes

There's no automatic migration of the old local `repos.json` to the
worker. Existing entries continue to record indexing stats (files,
symbols) as before; new repos register with the worker on the next
`recon init`. If you're already over your tier's `max_repos`, the
worker will reject new registrations until you `recon repos remove`
slots you no longer need.

[0.2.0]: https://github.com/bravo1goingdark/recon/releases/tag/v0.2.0

## [0.1.1] — 2026-04-25

### Fixed
- License HMAC secret mismatch between the CLI binary and the Cloudflare
  Worker. v0.1.0 shipped with `RECON_LICENSE_HMAC_KEY` (embedded in the
  binary at build time) and `LICENSE_HMAC_SECRET` (on the Worker)
  holding different values, so every `recon login` failed with
  `rejected: server response signature invalid or missing`. Both
  secrets have been rotated to the same value and v0.1.1 binaries
  validate licenses end-to-end.

[0.1.1]: https://github.com/bravo1goingdark/recon/releases/tag/v0.1.1

## [0.1.0] — 2026-04-24

First public release.

### Code intelligence (MCP server)

- Local-first Rust MCP server exposing five canonical tool shapes for
  Claude / Cursor / Windsurf / generic MCP clients.
- Tree-sitter backed symbol indexing across Rust, TypeScript, JavaScript,
  Python, Go, Java, C/C++, Ruby.
- Tantivy BM25 structured symbol search with a code-aware tokenizer.
- `fff-grep` hybrid search — lexical hits fused with symbol graph.
- Personalised PageRank repo-map using Aider-style edge weights.
- Incremental re-indexing driven by `gix` (file save → queryable in < 1 s).
- `cl100k_base` token counting so responses stay under the client context
  budget.
- `.recon/config.toml` for per-repo tuning; secret redaction and
  sensitive-path blocking on indexing.
- Release binary is stripped and under 30 MB across all targets.

### CLI + IDE integration

- `recon init --mcp cc|cursor|windsurf` writes the client's MCP config
  and verifies the binary launches cleanly over stdio.
- `recon login <key>` stores the license in a global credentials file;
  a single machine serves every repo on that account.
- `recon serve` — stdio MCP server, logs go to stderr only (stdout is
  strictly for MCP frames).
- End-to-end self-hosting test that spawns the real binary against this
  repo and validates tool descriptions + output shapes.

### Billing + subscriptions

- Razorpay Subscriptions with honour-until-period-end semantics:
  cancel records the intent, access continues to `current_period_end`,
  hourly cron downgrades the `api_keys` row once expired.
- Dual-currency pricing — USD globally, INR for subscribers in India
  (so UPI AutoPay / Net Banking eNACH work natively).
- PPP guard: `POST /v1/billing/subscribe` with `currency:"INR"` is 403
  unless Cloudflare `cf.country === "IN"`. Missing `cf` treated as
  non-IN so header stripping can't bypass.
- Webhook pipeline handles `subscription.{activated,charged,cancelled,
  halted,completed}` and `payment.captured` with idempotency via
  `payment_events(razorpay_payment_id PK)`.
- Account deletion cancels live Razorpay subscriptions immediately,
  then cascades across D1 (users → api_keys, sessions, payments,
  subscriptions + manual payment_events cleanup).
- Cron-driven tier downgrade runs hourly against expired `api_keys`.

### License validation

- HMAC-signed license cache on the client; revocation propagates to a
  running `recon serve` within 15 minutes.
- Single active API key per account — the worker rejects a second
  `POST /v1/dashboard/keys` with 409, forcing a revoke-and-regenerate
  rotation flow instead of silently stacking keys.

### Marketing site + dashboard

- Cloudflare Pages site at `mcprecon.pages.dev` with honest local-first
  positioning, token-economics data, and docs.
- Dashboard with three round icon tabs (Keys / Billing / Danger),
  dismissible quickstart panel persisted in `localStorage`, sticky
  footer, and themed in-page modals for revoke / cancel / delete (no
  browser `confirm()`).
- IP-geo'd currency defaults via a Pages Function reading
  `request.cf.country`; user can override except when overriding would
  grant PPP pricing they aren't eligible for.

### CI + release engineering

- Fast per-PR gates: rustfmt, clippy (`-D warnings`), linux-only test
  matrix, `cargo-audit`, `cargo-deny`, worker typecheck + Vitest.
- Heavy cross-platform + embed matrix gated on release tags + nightly
  schedule (`cross-platform.yml`) so PRs don't wait 40+ min on
  Windows/macOS runners or flaky `ort-sys` downloads.
- Release pipeline: five-target cross build (Linux x64/arm64, macOS
  x64/arm64, Windows x64) → `SHA256SUMS.txt` → keyless cosign signing
  via sigstore OIDC → R2 upload under `releases/<tag>/` → `latest.json`
  published → Pages deploy syncs `scripts/install.{sh,ps1}` into the
  site root.
- `install.sh` / `install.ps1` fetch the matching tarball, verify the
  SHA256, and optionally verify the cosign signature.

### Security

- Strict CSP on the Pages site (`script-src 'self'`, no
  `unsafe-inline`); every interactive element bound via
  `addEventListener`, dynamic rows use event delegation.
- OAuth redirect_uri computed from the browser-visible host so
  dev/staging/prod don't cross-contaminate.
- No embedding API calls to cloud providers by default (local ONNX only
  behind the `embed` feature).

[0.1.0]: https://github.com/bravo1goingdark/recon/releases/tag/v0.1.0
