# Changelog

All notable changes to this project are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the
project uses [SemVer](https://semver.org/).

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
