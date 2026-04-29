# Hosted embeddings — implementation plan

Branch: `feat/hosted-embed` (this file lives only on that branch until merged).
Tracking issue / FUTURE.md source: §1 of `FUTURE.md` (kept in sync —
when this work lands, delete the FUTURE.md section).

This file is the contract between current-you and future-you. Every
sub-component below has acceptance criteria; nothing is "done" until
the criteria pass on CI on this branch and the integration smoke test
runs end-to-end against a real Modal endpoint.

## Why this exists

Semantic search needs a code-embedding model. The local-inference path
(`recon-cli --features embed`) pulls fastembed → ort-sys (ONNX C++) →
hf-hub → native-tls → openssl-sys, plus a ~100 MB ONNX download on
first run. Concrete pain:

- Binary inflates from ~28 MB to ~80 MB.
- `aarch64-unknown-linux-gnu` cross-compile breaks on `openssl-sys`
  (needs an arm64 libssl sysroot in the cross runner).
- First-launch UX: the IDE looks frozen for ~30 s while the HF model
  downloads.
- Release matrix (`.github/workflows/release.yml`) is scoped to
  `-p recon-cli` specifically to dodge it.

Move inference to a hosted Modal service. Keep code-specialized
quality. Source files never leave the user's machine — only chunk
texts (tens of tokens, no storage, no logging on our side) travel.

## Architecture (reference)

```
┌──────────────────────────────────────────────────────────────┐
│ Client                                                        │
│   crates/recon-embed-client (new, ~150 LOC, ureq+rustls)      │
│     impl EmbedService { embed_batch(texts) -> Vec<Vec<f32>> } │
│     POST /v1/embed  Authorization: Bearer <api-key>           │
└──────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────┐
│ Cloudflare Worker                                             │
│   worker/src/routes/embed.ts (new)                            │
│     - requireApiKey middleware (existing)                     │
│     - tier-rate-limit:  RL_EMBED  Free=5k/day Pro=50k/day Team=∞│
│     - cache: EMBED_CACHE KV   key = "v1:" + sha256(text)      │
│         hit  → return cached vector, count toward rate-limit  │
│         miss → forward batch to Modal, write-through on 200   │
│     - failover: 503 with clear error if Modal is down (v1)    │
└──────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────┐
│ Modal                                                         │
│   modal/embed_service.py (new, single file)                   │
│     - Image: python:3.11 + transformers + torch               │
│     - GPU: T4 (cheapest CUDA, $0.60/hr, ~200 texts/s on Jina) │
│     - Model: jinaai/jina-embeddings-v2-base-code (Apache 2.0) │
│         loaded once via @modal.enter; warm container reuses it│
│     - Batch size: 64 — balances throughput vs first-byte time │
│     - scale_to_zero=True initially (free $30/mo credit)       │
│     - Returns List[List[float]] (768-dim per text)            │
└──────────────────────────────────────────────────────────────┘
```

## Component-by-component plan

### 1. Modal app — `modal/embed_service.py`

**Status**: not started.
**Files to create**:
- `modal/embed_service.py` — single-file Modal app.
- `modal/requirements.txt` — `transformers torch sentence-transformers`.
- `modal/README.md` — deploy command, token setup, URL hand-off.

**Skeleton** (intent — exact form may shift with Modal's current API):

```python
import modal

app = modal.App("recon-embed")

image = (
    modal.Image.debian_slim(python_version="3.11")
        .pip_install_from_requirements("requirements.txt")
)

# Bearer token shared with the Worker; rotate via `modal secret set`.
auth = modal.Secret.from_name("recon-embed-auth")

@app.cls(image=image, gpu="T4", scaledown_window=60, secrets=[auth])
class EmbedService:
    @modal.enter()
    def load_model(self):
        from sentence_transformers import SentenceTransformer
        self.model = SentenceTransformer(
            "jinaai/jina-embeddings-v2-base-code",
            trust_remote_code=True,
        )

    @modal.fastapi_endpoint(method="POST")
    def embed(self, payload: dict, authorization: str = ""):
        import os
        if authorization != f"Bearer {os.environ['MODAL_AUTH_TOKEN']}":
            return {"error": "unauthorized"}, 401
        texts = payload.get("texts", [])
        if not isinstance(texts, list) or len(texts) > 64:
            return {"error": "texts must be a list of <= 64 strings"}, 400
        vectors = self.model.encode(texts, batch_size=64, normalize_embeddings=True)
        return {"vectors": [v.tolist() for v in vectors]}
```

**Acceptance criteria**:
- `modal deploy` succeeds.
- `curl -X POST $MODAL_URL/embed -H "Authorization: Bearer …" -d '{"texts":["fn x(){}"]}'`
  returns 200 with `vectors` of length 1, each 768 floats.
- Wrong bearer → 401.
- Cold start under 7 s; warm latency under 250 ms for batch of 32.
- Container scales to zero after 60 s idle.

**Open question — kept warm?** Start with `scale_to_zero=True`. If
real-user feedback flags 5 s cold start as too slow, switch to
`min_containers=1` (~$14/mo for one T4 held idle 24/7) OR add a
10-min warm-up cron from the Worker during business hours. Don't
prematurely optimize.

### 2. Worker route — `worker/src/routes/embed.ts`

**Status**: not started.
**Files to create**:
- `worker/src/routes/embed.ts` — the new route.
- `worker/tests/embed.test.ts` — vitest coverage.

**Files to modify**:
- `worker/src/index.ts` — mount `embedRoutes` at `/v1/embed` and
  `/api/v1/embed` (mirroring existing dashboard / billing pattern).
- `worker/wrangler.toml` — add `RL_EMBED` rate-limit binding +
  `EMBED_CACHE` KV namespace + `MODAL_EMBED_URL` env var (the
  Modal-deployed URL) + `MODAL_AUTH_TOKEN` secret (shared with Modal).
- `worker/src/types.ts` — add `RL_EMBED?: RateLimitBinding` and
  `EMBED_CACHE?: KVNamespace` and `MODAL_EMBED_URL?: string` and
  `MODAL_AUTH_TOKEN?: string`.

**Behavior**:
- Auth via existing `requireApiKey` middleware.
- Body schema: `{ texts: string[] }` with `1 <= len <= 64`. Each
  string ≤ 8192 chars (the model's context window).
- Rate-limit per (api-key-prefix, day-bucket): Free 5 000 chunks/day,
  Pro 50 000, Team unlimited. Reuse `lookupBinding` pattern.
- Cache hit path (KV): return immediately. Cache miss: batch the
  uncached texts, POST to `MODAL_EMBED_URL/embed` with bearer auth,
  write-through on 200 (TTL 30 days), reassemble in original order.
- Modal failure (5xx, timeout >10 s): return 503 with
  `{"error":"embed_service_unavailable","retry_after":30}`. Client
  falls back to lexical-only.

**Acceptance criteria**:
- 14 vitest cases in `embed.test.ts`:
  - 401 without auth
  - 200 + cached on second identical request
  - 429 over per-tier limit (Free at 5001th/day)
  - Pro/Team can exceed Free's cap
  - 400 on `texts: []` and on `texts: [over 64 elements]`
  - 400 on string longer than 8192 chars
  - 503 when Modal mock returns 5xx
  - 503 when Modal mock times out (>10 s)
  - Cache write-through (KV `put` called once per uncached text)
  - Mixed-cache batch (3 cached + 5 fresh = 1 Modal call for 5)
  - Cache key uses `v1:` prefix + lowercase hex sha256
  - Auth-key tier read from `api_keys.tier` (mocked in setup)
  - Modal request includes `Authorization: Bearer $MODAL_AUTH_TOKEN`
  - Response shape: `{ vectors: number[][] }` 768-dim verified

### 3. Rust client — `crates/recon-embed-client/`

**Status**: not started.
**Files to create**:
- `crates/recon-embed-client/Cargo.toml`
- `crates/recon-embed-client/src/lib.rs`

**Files to modify**:
- `Cargo.toml` (workspace) — add `recon-embed-client = { path = "crates/recon-embed-client" }`.
- `crates/recon-cli/Cargo.toml` — depend on `recon-embed-client`,
  remove the `embed` feature default.
- `crates/recon-server/Cargo.toml` — gate the existing `recon-embed`
  dep behind `local-embed` feature, add `recon-embed-client` as the
  default path.
- `crates/recon-server/src/server.rs` — swap the
  `#[cfg(feature = "embed")]` blocks. Hosted is the default; local
  ONNX is opt-in via `--features local-embed`.

**Public surface** (matches the existing `EmbedService` trait shape so
`recon-server` swaps cleanly):

```rust
pub struct HostedEmbedService {
    base_url: String,           // https://api.recon.dev (or self-hosted worker)
    api_key: String,            // user's recon API key
    agent: ureq::Agent,
}

impl HostedEmbedService {
    pub fn new(base_url: String, api_key: String) -> Self;
    pub fn from_env() -> Result<Self>;   // reads RECON_API_URL + credentials.json
}

impl EmbedService for HostedEmbedService {
    fn embed_batch(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, Error>;
    fn vector_dim(&self) -> usize { 768 }
}
```

**Privacy escape hatch**: respect `RECON_NO_EMBED=1` — when set,
`HostedEmbedService::from_env` returns `None` (or an `EmbedDisabled`
sentinel) and `recon-server` falls back to lexical-only. Document
prominently in CLAUDE.md and the embed-feature section of Docs.

**Acceptance criteria**:
- `cargo build -p recon-embed-client` succeeds with zero `openssl`
  features pulled (verify `cargo tree -p recon-embed-client | grep -i openssl`
  is empty; `rustls` only).
- ~6 unit tests:
  - `from_env` reads `RECON_API_URL` env var (with default fallback).
  - `from_env` reads credentials file via the existing
    `recon_server::license::read_credentials` helper.
  - `embed_batch([])` returns `Ok(vec![])` without an HTTP call.
  - `embed_batch([>64 elements])` chunks into multiple HTTP calls.
  - Auth header carries `Bearer <api-key>`.
  - 503 response surfaces a typed `Error::ServiceUnavailable` so
    callers can fail-closed cleanly.
- An integration test (mocked HTTP) exercises a 32-string batch
  end-to-end against a stub server.

### 4. recon-server wiring

**Files to modify**:
- `crates/recon-server/Cargo.toml` — features rework:
  ```toml
  [features]
  default = []                           # hosted is the default code path
  local-embed = ["dep:recon-embed"]      # opt-in offline / air-gapped
  ```
- `crates/recon-server/src/server.rs`:
  - The `embed_service`, `vec_read_pool`, `vec_writer` fields stay
    `cfg(feature = "...")`-gated, but the cfg flips so `local-embed`
    enables the legacy fastembed path while the default path uses
    `recon-embed-client::HostedEmbedService`.
  - Watcher's embed-catch-up + per-batch embed paths swap to the
    hosted service. The shape (`format_symbol(&sym, &body)` →
    `Vec<f32>`) is identical, so this is a type swap not a logic
    rewrite.
  - Vector storage stays local: `VectorStore` (sqlite-vec) is
    unchanged. Only the *generator* moves to hosted.

**Acceptance criteria**:
- `cargo build -p recon-cli` (no features) ships the hosted client by
  default, binary stays under 30 MB stripped.
- `cargo build -p recon-cli --features local-embed` still works for
  air-gapped users; matches v0.2.x size and behavior.
- `cargo test --workspace` clean.
- Watcher embed catch-up logs the chunks-per-second throughput and
  stops on `RECON_NO_EMBED=1`.

### 5. Site + docs + CLAUDE.md

**Files to modify**:
- `site/index.html` — landing copy. Replace "Code never leaves your
  machine" framing with a more honest version: source files stay
  local; index chunks (tens of tokens, no storage, no logging) are
  sent to our embed service for vectorization. Air-gapped opt-out
  available via `RECON_NO_EMBED=1` or `--features local-embed` rebuild.
- `site/Docs.html` — add a "Privacy & embeddings" section spelling out
  what travels, what doesn't, and the two escape hatches.
- `site/pricing.html` — note the per-tier embed quotas (Free 5k/day,
  Pro 50k/day, Team unlimited) on the feature comparison.
- `CLAUDE.md` — replace the rule:
  > "No embedding API calls to cloud providers by default (local ONNX only)."
  with:
  > "Embeddings flow through the recon hosted service by default
  > (chunk text only — no source files, no logs). Offline ONNX is the
  > opt-in escape hatch via `--features local-embed` or
  > `RECON_NO_EMBED=1`."

### 6. CHANGELOG

When ready to ship, add a v0.4.0 (or whatever the next minor is)
entry under `### Added`:

> Semantic search re-enabled by default via a hosted embedding
> service (`recon-embed-client` crate, `recon-api.../v1/embed`
> route, Modal-hosted Jina v2-base-code). Source files stay local;
> only chunk texts (tens of tokens, no storage, no logging on our
> side) travel for vectorization. Air-gapped users can rebuild with
> `--features local-embed` for offline inference via fastembed, or
> set `RECON_NO_EMBED=1` to skip embeddings entirely (semantic
> search degrades to lexical-only; everything else is unaffected).
> Per-tier quotas: Free 5 000 chunks/day, Pro 50 000/day, Team
> unlimited.

## Implementation order

Start with Modal because everything downstream needs a URL to point at.

1. **Modal app + deploy**. Get a working URL + bearer token. Smoke-test
   with curl. ~½ day.
2. **Worker route** — implement, write the 14 vitest cases first
   (failing), make them pass. KV binding + rate-limit binding +
   secrets all configured via wrangler. ~1 day.
3. **Rust client crate**. Pure ureq+rustls. ~½ day.
4. **Wire into recon-server** behind the new cfg gate. ~½ day.
5. **Site + CLAUDE.md + docs** copy changes. ~½ day.
6. **`wrangler deploy`**, then merge `feat/hosted-embed` → `main`,
   tag v0.4.0, release.

Total ~3 days of focused work. Most of the risk is in (2) — getting
the cache + rate-limit + failover semantics right.

## Resolved open questions

| Q | Decision |
|---|---|
| Cache key shape | `v1:` + `sha256(text)`. Model upgrade → bump prefix to `v2:`. Don't include model_id in the hash itself; it's noise when we only ship one model. |
| `RECON_NO_EMBED` env var | Yes. Lands with this work, documented next to `--features local-embed`. |
| Failover when Modal is down | v1: 503 fail-closed. Add Jina commercial API ($0.02/1M) as secondary only after we see a real Modal outage in production. |
| Vector storage | Stays local (sqlite-vec). Embeddings flow through hosted service; vectors live with the user. |
| Model swap policy | Don't. One model, one dim, one migration story. If we swap, the prefix bump (`v1:` → `v2:`) invalidates the cache and `recon-server` triggers a rebuild on next watcher catch-up. |

## Non-goals

- **Self-hosted GPU** (Runpod, Fly, Lambda). Modal's free tier covers
  v0.4 validation scale. Revisit when we exceed ~$100/mo.
- **Fine-tuning** the model on user data. Wait until we have real
  query/code pairs.
- **Provider pluggability**. Single model, single provider for v1.
- **Hosting vectors server-side**. Burns D1 quota and regresses the
  privacy story.

## Rollback plan

If hosted embed turns out to be a mistake (cost blow-up, privacy
pushback, latency complaints):

- **Worker**: remove the `/v1/embed` route + `wrangler deploy`. Old
  clients get 404 → semantic search fails closed → falls back to
  lexical (graceful degradation).
- **Modal**: `modal app stop recon-embed`. Container stops billing
  immediately.
- **Client**: next release ships without `recon-embed-client`,
  reverts the cfg gate to make `--features local-embed` (or its
  successor) the default.
- **Cache**: KV entries TTL out at 30 days; nothing to clean up.

The worst case is a 2-week period where some users are on a binary
that 404s on `/v1/embed`. Still safe — semantic features degrade,
lexical search keeps working, no data loss.

## Branch policy

`feat/hosted-embed` is a long-running feature branch. Rebase onto
`main` before each focused work session — short-lived `main`
shouldn't drift much from this branch's base.

When merging:
1. Rebase onto `main` to get a linear history.
2. Squash to ≤6 commits (one per implementation-order step above).
3. Delete `FUTURE.md` §1 in the same merge commit (per the file's
   own contract).
4. Tag the next version (probably v0.4.0) on `main` post-merge.

## Smoke-test checklist (before merging to main)

Run end-to-end against the *real* Modal deployment + dev Worker:

- [ ] `recon init` against a fresh repo → first save triggers embed
      catch-up, 100 chunks/sec sustained throughput.
- [ ] `code_find_symbol --semantic "implements EmbedService"` returns
      relevant hits across the Rust source.
- [ ] `RECON_NO_EMBED=1 recon serve` skips embed entirely; lexical
      search still works.
- [ ] `cargo build --features local-embed -p recon-cli` produces a
      binary that uses fastembed (verify with `strings | grep onnxruntime`).
- [ ] CF rate-limit kicks in at 5001st chunk on a Free key (responses
      are 429 with the upsell payload).
- [ ] Cache hit ratio > 70% on the second run of the same repo
      (verify via Worker tail).
- [ ] Modal scale-to-zero confirmed: 10 minutes idle → next request
      cold-starts in <7 s.
- [ ] `cargo fmt + clippy -D warnings + cargo test --workspace`
      across all feature combinations: `(default)`, `local-embed`.
