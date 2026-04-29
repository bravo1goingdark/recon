# Modal — recon embed service

This directory holds the Python app deployed to [Modal](https://modal.com)
that hosts the embedding model used by the `/v1/embed` Worker route.

**This is a scaffold, not a deployment.** See
`docs/HOSTED_EMBED_PLAN.md` §1 for the full implementation plan.

## Layout (planned)

- `embed_service.py` — single-file Modal app. Loads
  `jinaai/jina-embeddings-v2-base-code` once per container, exposes
  `POST /embed`, returns 768-dim vectors. Bearer-auth shared with
  the Worker.
- `requirements.txt` — `transformers torch sentence-transformers`.

## Deploy (once `embed_service.py` exists)

```sh
# First time:
modal token new
modal secret create recon-embed-auth MODAL_AUTH_TOKEN=$(openssl rand -hex 32)

# Each deploy:
modal deploy embed_service.py
```

Modal prints the public URL on success. Set it as `MODAL_EMBED_URL`
on the Worker via:

```sh
echo "https://<your-app>.modal.run" | wrangler secret put MODAL_EMBED_URL
```

And mirror the bearer token to the Worker:

```sh
echo "$(openssl rand -hex 32)" | wrangler secret put MODAL_AUTH_TOKEN
# (same token then set as the Modal secret)
```

## Notes

- Cold start: ~3–5 s for first request after scale-to-zero (model
  loads from `/models` cache once per container).
- Warm latency: ≤250 ms for batch of 32 on T4.
- Cost ceiling: $30/mo Modal Starter free credit covers ~900 users at
  the projected 80/15/5 Free/Pro/Team mix.
