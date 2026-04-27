# Changelog

All notable changes to this project are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the
project uses [SemVer](https://semver.org/).

## [0.2.3] — 2026-04-27

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
