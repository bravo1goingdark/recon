# Future work

Design briefs for features that are decided but not yet implemented.
Each section is self-contained so a future contributor doesn't need the
conversation that birthed it. When you start a section, move it to
`docs/IMPLEMENTATION.md`; delete it here once shipped.

---

## Hosted embeddings (target: v0.3.0)

### Problem

Semantic search needs a code-embedding model. v0.1.x and v0.2.x ship
*without* it because the local-inference path (`--features embed`) pulls:

- `fastembed` → `ort-sys` (ONNX Runtime, C++ lib)
- `fastembed` → `hf-hub` / `reqwest` → `native-tls` → `openssl-sys`
- ~100 MB ONNX model downloaded on first run

This inflates the binary, breaks cross-compile to `aarch64-linux`
(openssl-sys needs an arm64 libssl sysroot), and creates a bad
first-launch UX (huge HF download). `cargo build --release` in the
release matrix is scoped to `-p recon-cli` specifically to avoid it.

### Design

Move inference to the server. Keep a code-specialized model; keep the
quality; eliminate the dep weight on the client.

```
Client (new crate: recon-embed-client)
  │ POST /v1/embed  { texts: [...] }
  │ Authorization: Bearer <api-key>
  ▼
Worker route  POST /v1/embed  (worker/src/routes/embed.ts)
  - Auth via license.ts key lookup (same pattern as /v1/license/validate)
  - Tier-rate-limited:   Free=5k/day, Pro=50k/day, Team=unlimited
  - Cache in CF KV by SHA256(text): same chunk across users hits Modal once
  - Fails over to a secondary provider on Modal 5xx
  │ POST https://<our-app>.modal.run/embed  (bearer auth)
  ▼
Modal container  (modal/embed_service.py)
  - Model: jinaai/jina-embeddings-v2-base-code  (Apache 2.0)
  - T4 GPU, scale-to-zero
  - Batch size 64; cold-start ~3-5s (model is ~320 MB)
  - Returns List[List[float]]  (768-dim per text)
```

### Why Jina v2-base-code

Considered: CF Workers AI bge-small-en-v1.5, Salesforce SFR-Embedding-Code-400M_R,
Voyage voyage-code-2, Qodo-Embed-1.

Picked Jina v2-base-code because:

- **Apache 2.0** — verified on the HF model card. Self-hostable for a
  commercial SaaS without licensing concerns.
- **Code-specialized** — trained on 30 programming languages. Materially
  better code-retrieval quality than general-English models like bge.
- **8192 token context window** — can embed *whole functions* as single
  chunks. Competitors top out at 512, which means chopping functions
  and losing the semantic link between signature, body, and comments.
- **161M params** — the smallest of the code-specific options; ~3 GB on
  GPU; faster inference and cheaper cold-start than SFR (400M) or
  Qodo (1.5B).

Don't swap silently. If a future contributor wants a different model,
document the rationale in CHANGELOG — embedding dimension changes force
a full re-embed of every indexed repo.

### Why Modal

Considered: CF Workers AI (can't bring your own model — catalog only),
CF Containers (GPU access is limited), Runpod, Fly.io GPU, HuggingFace
Inference Endpoints.

Picked Modal because:

- **$30/month recurring free credit on the Starter plan** — verified via
  pricing page and corroborated by real users (a Vijay post on X,
  ~2025-12, explicitly confirms "free $30 compute credit each month").
- **Scale-to-zero** — $0 when idle. No paying for an always-on GPU at
  v0.3 validation scale.
- **Cheap at scale** — T4 at $0.60/hr, ~200 texts/sec → ~$0.008 per 1M
  tokens if we exceed the free tier. Cheaper than Jina's commercial API
  ($0.02/1M) and Voyage ($0.05/1M).
- **Simple deployment** — Python app, `modal deploy`, done.

### Capacity ($30/mo budget)

| Tier | Typical chunks/month | Cost / user / month |
|------|----------------------|---------------------|
| Free | 1,000                | $0.0008             |
| Pro  | 130,000              | $0.11               |
| Team | 500,000              | $0.42               |

Realistic 80/15/5 (Free/Pro/Team) mix: ~900 users fit under $30/mo.
Other infra (CF Worker requests, KV writes) breaks first at ~100 DAU
on free-tier CF plans and needs $10/mo upgrades before Modal becomes
the bottleneck.

### Scope

**Worker:**
1. New route `worker/src/routes/embed.ts` exposing `POST /v1/embed`.
   Auth via the existing license.ts key-lookup pattern (reuse
   `sha256Hex` from `lib/crypto`).
2. Tier-rate-limit middleware — extend `middleware/ratelimit.ts` with
   a per-tier bucket (Free/Pro/Team caps as above). Use `RL_EMBED`.
3. CF KV binding `EMBED_CACHE` for chunk-hash → vector-json cache.
   Bind in `wrangler.toml`. Per-entry TTL: 30 days (chunks rotate as
   users re-index).
4. New secrets: `MODAL_EMBED_URL`, `MODAL_AUTH_TOKEN`. Add both to
   `gh secret list` expectations in `.github/workflows/deploy.yml` and
   to the wrangler.toml secret comment block.
5. Tests in `worker/tests/embed.test.ts` covering: 401 on no auth,
   200 + cached response on repeat, 429 on over-limit, passthrough to
   a mocked Modal endpoint.

**Modal:**
6. `modal/embed_service.py` — a single file Modal app. Load
   `jinaai/jina-embeddings-v2-base-code` once per container via
   `@modal.enter`. Endpoint batches input, returns vectors.
7. `modal/requirements.txt` — `transformers`, `torch`, `sentence-transformers`.
8. `modal/README.md` — deploy command, token setup, how to point the
   Worker at a new URL.
9. **Cold-start policy:** start with scale-to-zero. If users complain
   about the ~3-5s first-request latency, switch to `keep_warm=1`
   (~$14/mo for one container held idle 24/7) OR add a 10-minute
   warm-up cron from the Worker during active hours.

**Rust client:**
10. New crate `crates/recon-embed-client/` — pure `ureq + rustls` (no
    native deps). Implements the same `EmbedService` trait `recon-server`
    already expects from `recon-embed`. ~150 LOC.
11. `recon-cli/Cargo.toml` — enable hosted embedding by default.
12. Demote `recon-embed` to an opt-in `--features local-embed` for
    offline / air-gapped users who'd rather run ONNX locally.
13. `recon-server/src/server.rs` — swap the `#[cfg(feature = "embed")]`
    gates so `recon-embed-client` is the default path and the legacy
    fastembed path only compiles under `--features local-embed`.

**Site + docs:**
14. Update marketing copy on `site/index.html`: "Code never leaves your
    machine" → "Source files stay local. Index chunks (tens of tokens,
    no storage, no logging) are sent to our embed service for
    vectorization." Pricing page footer similarly.
15. Update `CLAUDE.md` rule: "No embedding API calls to cloud providers
    by default (local ONNX only)." → reflect that hosted is the default,
    `--features local-embed` is the offline escape hatch.

**CHANGELOG:**
16. v0.3.0 entry under `### Added` — honest framing: "Semantic search
    re-enabled via a hosted embedding service. Source code stays local;
    index chunks are sent for vectorization. Air-gapped users can build
    with `--features local-embed` for offline inference via fastembed."

### Implementation order

1. Modal app + deploy — Python is self-contained; start here so
   everything downstream has a URL to point at.
2. Worker route + KV binding + tests.
3. Rust client crate (recon-embed-client).
4. Wire into recon-server behind the new cfg gate.
5. Site + CLAUDE.md + CHANGELOG updates.
6. Deploy Worker, tag v0.3.0, re-release.

### Open questions

- **Caching boundary:** hash `text` alone or `(text, model_id)`? Model
  upgrades that return different-dim vectors would poison a single-key
  cache. Leaning toward `(model_id, sha256(text))`. Decide before the
  KV binding ships — migrating keys later is painful.
- **Privacy posture:** we currently position as "local-first." Hosted
  embed means chunks leave the machine. Do we need a
  `RECON_NO_EMBED=1` env var that disables the feature client-side for
  users who need it off? Probably yes — document next to the
  `--features local-embed` escape hatch.
- **Provider failover:** spec says "fail over to a secondary on Modal
  5xx" but doesn't name the secondary. Options: Jina commercial API
  (~$0.02/1M, already discussed), Voyage API, or simply serve stale
  cache + return 503 to the client if no secondary configured. Easiest
  start: 503 with a clear error; add Jina as secondary when Modal has a
  real outage and we decide failover is worth the code.

### Non-goals for v0.3.0

- **Self-hosted GPU** (Runpod, Fly, etc.) — Modal's free tier covers us.
  Revisit when Modal exceeds ~$100/mo.
- **Fine-tuning the model on user data** — wait until we have real
  query/code pairs to train on.
- **Embedding models beyond Jina v2-base-code** — one model, one dim,
  one migration story. Add provider pluggability after the single-model
  path proves out.
- **Vector DB beyond sqlite-vec on the client** — keep the vector store
  local. Hosting vectors server-side would regress the privacy story
  and burn D1 write quotas. Only embeddings travel; storage stays local.

### Rollback plan

- Worker: remove the `/v1/embed` route. Old clients with the
  `recon-embed-client` crate get 404 → semantic search fails closed,
  falls back to lexical.
- Modal: `modal app stop recon-embed` — container stops charging.
- Client: next release ships without `recon-embed-client`, wires
  `--features local-embed` back to default.

---

## Measured token-savings (target: v0.4.0)

### Problem

The "tokens saved" headline on `mcprecon.pages.dev/dashboard` is built
from a static `BASELINES` table at
`crates/recon-server/src/telemetry.rs:60-161`. Each tool has a
hard-coded "what Read+grep would have cost" number (e.g.
`code_outline → 3000`, `code_search → 4000`, `code_repo_map → 20000`).
The diff between actual `response_tokens` and that constant is the
headline savings number, displayed under a `"Estimate, not a
measurement."` disclaimer.

That makes a marketing claim depend on numbers we made up. Replace
the static baseline with a per-call *measurement* of the actual
Read/grep equivalent so the dashboard can drop the asterisk.

### Design

Inline measurement, not a `BaselineMeasurer` trait. Bucket-1 handlers
that already touch the bytes the alternative would have read can
capture `estimate_tokens(content) as u64` cheaply. A trait would
re-do that I/O, breaking the <100ms p99 latency target in `CLAUDE.md`.

Reuse the existing token counter at `crates/recon-search/src/tokens.rs`
(`estimate_tokens` → tiktoken cl100k_base BPE with chars/4 fallback)
— same function used for `response_tokens`, so the two numbers are
directly comparable.

**Pre-launch posture**: there are no users yet, so no back-compat
machinery. Ship measurement always-on, drop the legacy `baseline_tokens`
column from the wire/dashboard, delete the migrated entries from
`BASELINES` in the same PR. The static table only retains entries for
tools that have no honest alternative.

Tools split into two buckets:

1. **Has a clean alternative** (9 tools, all measured): `code_outline`,
   `code_skeleton`, `code_read_symbol`, `code_search`, `code_find_symbol`,
   `code_find_refs`, `code_find_strings`, `code_multi_find`, `code_list`.
2. **No clean alternative** (11 tools, stay estimated): the composite
   tools (`code_path`, `code_callers`, `code_callees`, `code_context`,
   `code_impact`, `code_subsystem`, `code_subsystems`) and the
   no-alternative ones (`code_repo_map`, `code_stats`, `code_reindex`,
   `code_savings`). These keep their static baseline. Dashboard surfaces
   their savings under a separate "estimated (composite tools)" line.

### Status (as of 2026-04-29)

The first pass landed a back-compat-flavoured plumbing layer (3
measured fields alongside the legacy ones, `RECON_MEASURED_BASELINES`
opt-in flag, "Measured / Mostly estimated" badge, additive D1
migration). With no users to support, that's overbuilt — the cleanup
below collapses it into a single ship.

### Scope (single ship, no staged rollout)

1. **`telemetry.rs` cleanup.** Replace `baseline_tokens` with
   `measured_baseline_tokens` for the 9 bucket-1 tools. The remaining
   11 tools keep their static baseline under a renamed
   `static_baseline_tokens` field so the source of each number is
   explicit. Drop the dual-track `measured_response_tokens` /
   `measured_calls` machinery — every bucket-1 call is measured, so
   the slice is the whole population. Drop the legacy-row hydration
   test.
2. **`RECON_MEASURED_BASELINES` flag — delete it.** Measurement is
   always on. Keep an emergency kill-switch only if the latency bench
   in step 9 shows a real risk.
3. **`server.rs`.** Migrate the remaining 6 bucket-1 handlers
   (`code_search`, `code_find_symbol`, `code_find_refs`,
   `code_find_strings`, `code_multi_find`, `code_list`) to
   `instrumented_measured`. For the search-flavoured ones, accumulate
   a running sum of `estimate_tokens(match_line)` during the existing
   search pass *before* truncation — captures what an unbounded grep
   would have emitted with no second pass and no extra I/O. Cap the
   sum at ~5 MB-of-tokens to bound worst case on huge repos. Touches
   `crates/recon-search/`.
4. **`crates/recon-cli/src/savings.rs`.** Push wire shape becomes
   `{day, calls, response_tokens, measured_baseline, static_baseline,
   tokens_saved, latency_micros}` — one measured number, one static
   number, one combined `tokens_saved`. Drop `measured_calls` etc.
5. **D1 schema.** Edit migration `0011` in place (no users, safe to
   rewrite): two columns `measured_baseline_tokens` and
   `static_baseline_tokens`, both `INTEGER NOT NULL DEFAULT 0`. Drop
   the old `baseline_tokens` column. (If `0011` has already been
   applied to production D1 by the time this lands, swap to a `0012`
   that drops `baseline_tokens` and adds the two replacements.)
6. **Worker route + dashboard.** POST validators require both new
   fields. GET response returns them plus a derived `tokens_saved`.
7. **Dashboard UX.** Drop the "Estimate, not a measurement"
   disclaimer entirely. Single headline (`measured_tokens_saved`),
   single line under it: `Read+grep equivalent measured per-call against
   the in-process index.` A second small line for composite tools:
   `Composite tools (code_repo_map, code_path, etc.) still use static
   baselines.` Drop the badge / split-row machinery from v0.

### Verification

8. Integration test `crates/recon-server/tests/measured_baselines.rs`
   — spin up `ReconServer` against a fixture repo, invoke each of
   the 9 bucket-1 tools, assert `measured_baseline_tokens >
   response_tokens` for ≥7 of 9. Also assert MCP response shape
   unchanged.
9. Latency benchmark in `crates/recon-search/benches/search_bench.rs`
   for `code_search`-shaped workloads. **Gate: <5% to median, <10%
   to p99 vs the pre-measurement build.** If the running-sum
   approach fails the bench, fall back to clipping at first 1 MB of
   match bytes and extrapolating.
10. Calibration sanity-check via a new xtask
    `cargo xtask measured-baselines-corpus --repo <path>`. For each
    of the 9 measured tools, capture
    `(measured_baseline, response_tokens, ratio)`, emit a TSV. The
    output is a design artifact, not a gate — used to confirm that
    measured numbers don't expose anything weirder than a 5–10×
    savings ratio per tool. Run on `intel`, `rust-main`, and one
    smaller fixture before declaring v0.4.0 ready.

### Critical files

- `crates/recon-server/src/telemetry.rs`
- `crates/recon-server/src/server.rs`
- `crates/recon-cli/src/savings.rs`
- `worker/migrations/0011_usage_rollups_measured.sql` (rewrite)
- `worker/src/routes/account.ts`
- `worker/src/routes/dashboard.ts`
- `site/dashboard/index.html`, `site/js/dashboard.js`
- `crates/recon-server/tests/measured_baselines.rs` (new)
- `xtask/src/measured_baselines_corpus.rs` (new)

### Open questions

- **Search-pass running sum cap.** Spec says ~5 MB-of-tokens. Is that
  right? Decide by running step 10's calibration harness against
  `rust-main` and `zed-main` and picking the smallest cap that
  doesn't truncate >5% of real calls.
- **Composite-tool measurement.** `code_path`, `code_context`,
  `code_impact`, etc. — measuring them honestly means simulating
  "what N chained calls would have cost." Data-dependent, re-runs
  work. Stays on static baselines forever unless someone proves a
  cheap measurement.

### Non-goals

- **Measuring composite tools.** Static `BASELINES` entries stay for
  the 11 non-bucket-1 tools.
- **A "raw transcript" measurement.** Capturing actual agent
  input/output token counts from the IDE would be the gold standard,
  but Claude Code / Cursor / Windsurf don't expose it to MCP servers.
  Out of scope.

### Rollback plan

- Server: revert the PR. Static `BASELINES` was preserved for the 11
  non-measured tools, so the path back to "all estimated" is just
  re-adding the bucket-1 entries.
- Worker: revert the migration. With no users, dropping the new
  columns and restoring `baseline_tokens` is safe.
- Dashboard: revert the disclaimer. Same UX as today.
