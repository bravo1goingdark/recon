"""
recon-embed — hosted embedding service for the recon MCP server.

Single-file Modal app. The matching design brief lives at
`docs/HOSTED_EMBED_PLAN.md` §1 in this repo; read that first for the
why (cost ceiling, model choice, privacy posture, failover policy).

Surface
-------
POST /embed
    Headers: Authorization: Bearer <MODAL_AUTH_TOKEN>
    Body:    {"texts": ["...", "..."]}
    Returns: {"vectors": [[float; 768], ...]}    on 200
             {"error": "..."}                    on 400/401/500

Auth
----
Bearer token in `MODAL_AUTH_TOKEN` (Modal secret). The Cloudflare
Worker holds the same token in `MODAL_AUTH_TOKEN` (Wrangler secret)
and adds it to every forwarded request. Rotate by updating both.

Model
-----
jinaai/jina-embeddings-v2-base-code (Apache 2.0, 161M params,
8192-token context, 768-dim output). One model, one dim — swapping
forces a coordinated cache + index rebuild, see the plan's "model
swap policy" row.

Lifecycle
---------
- Container starts, `@modal.enter` loads the model into GPU RAM
  (~3-5 s on T4 from cold).
- Subsequent requests reuse the loaded model (~250 ms for batch=32).
- After 60 s idle, container scales to zero. Next request
  cold-starts again.

Deploy
------
    modal token new                                   # one-time
    modal secret create recon-embed-auth \\
        MODAL_AUTH_TOKEN=$(openssl rand -hex 32)
    modal deploy embed_service.py
    # → prints https://<your-app>.modal.run
"""
from __future__ import annotations

import os
from typing import List

import modal

# ── Modal app + image ────────────────────────────────────────────────
# Build the image from the pinned requirements.txt so cold-starts use
# a deterministic dep set. The python_version pin guards against
# Modal silently bumping to a 3.13 default that breaks our deps.
app = modal.App("recon-embed")

image = (
    modal.Image.debian_slim(python_version="3.11")
    .pip_install_from_requirements("requirements.txt")
    .env({"PYTORCH_CUDA_ALLOC_CONF": "expandable_segments:True"})
)

# Deferred imports: only available inside the Modal container, not on
# the local Python that runs `modal deploy`. `Request` is used as a
# parameter type annotation; FastAPI injects the live request so we
# can read the `Authorization` header off it manually. (We can't use
# the `Header(default="")` default-arg pattern here because Python
# evaluates default-value expressions at function-definition time —
# which on a `modal deploy` run is the local interpreter, where
# fastapi isn't importable.) `JSONResponse` is used for explicit
# status codes because `@modal.fastapi_endpoint` flattens tuple
# returns like `(body, 401)` into a JSON list with HTTP 200 —
# silently the wrong thing for callers that branch on status code.
#
# `from __future__ import annotations` (top of file) makes every
# type annotation in this module a string forward-ref, so
# `request: Request` doesn't try to resolve `Request` until FastAPI's
# endpoint registration runs inside the container.
with image.imports():
    from fastapi import Request
    from fastapi.responses import JSONResponse

# Bearer token shared with the Worker. Rotate via:
#   modal secret create --force recon-embed-auth MODAL_AUTH_TOKEN=<new>
auth_secret = modal.Secret.from_name("recon-embed-auth")


# ── EmbedService ─────────────────────────────────────────────────────
# Class-form Modal app so the model loads once per container (in
# `@modal.enter`) and every request reuses it. Function-form would
# reload per request — fine for cheap models, painful for our 320MB
# Jina checkpoint.
@app.cls(
    image=image,
    gpu="T4",                    # cheapest CUDA on Modal, $0.60/hr
    scaledown_window=60,         # idle 60s → container stops billing
    secrets=[auth_secret],
    timeout=120,   
                                # one request shouldn't outlast model load + 100s
)
class EmbedService:
    """Wraps the Jina model with a tiny FastAPI surface."""

    @modal.enter()
    def load_model(self):
        # Imported inside the function so the local module-level
        # imports stay tiny. Modal runs this once per container at
        # startup; the model lives on the GPU until the container
        # scales down.
        from sentence_transformers import SentenceTransformer  # type: ignore

        self.model = SentenceTransformer(
            "jinaai/jina-embeddings-v2-base-code",
            trust_remote_code=True,    # Jina ships custom modeling code
        )
        # Warm the kernels with a dummy batch so the very first real
        # request doesn't pay for CUDA-graph compilation. Throwaway
        # output. Cost: ~200 ms once per container.
        self.model.encode(["pub fn warmup() {}"], normalize_embeddings=True)

    # `requires_proxy_auth=False` opts out of Modal's account-level
    # OAuth gate. Without this, every request hits a 303 redirect to
    # Modal's login flow before our Bearer-check ever runs (the
    # gateway returns 303, not 401). We take the gate ownership
    # ourselves: only `MODAL_AUTH_TOKEN`-bearing requests reach
    # `embed`. Verified in v0.4 smoke-test commit history.
    @modal.fastapi_endpoint(method="POST", requires_proxy_auth=False)
    def embed(self, payload: dict, request: Request):
        """
        Authenticate the caller, validate the batch shape, run the
        model, return 768-dim L2-normalised vectors.

        Validation matches the Worker's contract so the Worker can
        rely on Modal returning 400 for malformed requests rather
        than silently doing the wrong thing.
        """
        authorization = request.headers.get("authorization", "")
        expected = f"Bearer {os.environ.get('MODAL_AUTH_TOKEN', '')}"
        if expected == "Bearer " or authorization != expected:
            return JSONResponse(
                content={"error": "unauthorized"}, status_code=401
            )

        texts = payload.get("texts")
        if not isinstance(texts, list) or not all(isinstance(t, str) for t in texts):
            return JSONResponse(
                content={"error": "texts must be a list of strings"},
                status_code=400,
            )
        if len(texts) == 0:
            return JSONResponse(content={"vectors": []}, status_code=200)
        if len(texts) > 64:
            return JSONResponse(
                content={"error": "batch size must be <= 64"},
                status_code=400,
            )
        # Jina v2 base code's context window is 8192 tokens; 8192
        # *characters* is a safe under-estimate (~2KB). Reject longer
        # so we don't truncate silently.
        for i, t in enumerate(texts):
            if len(t) > 8192:
                return JSONResponse(
                    content={
                        "error": f"texts[{i}] exceeds 8192-character limit"
                    },
                    status_code=400,
                )

        # Clear fragmented VRAM from previous requests to avoid OOM
        import torch
        torch.cuda.empty_cache()

        vectors: List[List[float]] = self.model.encode(
            texts,
            batch_size=8,
            normalize_embeddings=True,   # so cosine ~= dot product downstream
            convert_to_tensor=False,     # plain Python lists in the JSON
            show_progress_bar=False,
        ).tolist()

        return JSONResponse(
            content={"vectors": vectors}, status_code=200
        )


# ── Local-dev convenience ────────────────────────────────────────────
# `modal run embed_service.py` exercises the full lifecycle without
# `modal deploy`. Useful when iterating on the validation logic; not
# part of the deployment surface.
@app.local_entrypoint()
def main():
    print("To deploy:    modal deploy embed_service.py")
    print("To smoke-test against the deployed URL once it's up:")
    print(
        '    curl -X POST "$MODAL_EMBED_URL/embed" \\\n'
        '         -H "Authorization: Bearer $MODAL_AUTH_TOKEN" \\\n'
        '         -H "Content-Type: application/json" \\\n'
        '         -d \'{"texts": ["pub fn baseline_for(tool: &str) -> u64"]}\''
    )
