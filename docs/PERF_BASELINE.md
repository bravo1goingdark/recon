# Perf baseline — pre-changes

Captured 2026-04-29 on the working branch before the bundled perf pass.
Hardware: linux x86_64, 6.17.0-22-generic. Bench tool: criterion default
config with `--measurement-time 4 --warm-up-time 1`.

## Storage benches (`cargo bench -p recon-storage`)

| Bench                                  | Time        | Note                                           |
|----------------------------------------|-------------|------------------------------------------------|
| `search_symbols_fuzzy/10k`             | 161.71 µs   | unaffected by this work                        |
| `insert_symbols_batch/1k`              | 50.31 ms    | unaffected by this work                        |
| `all_symbols/80k_across_1780_files`    | **161.89 ms** | Cache snapshot cost — Phase 3 target         |
| `all_refs/300k_across_1780_files`      | **185.90 ms** | Cache snapshot cost — Phase 3 target         |
| `delete_cascade_loop/100_files`        | **76.02 ms**  | Phase 1 baseline (loop-of-N transactions)    |
| `delete_cascade_loop/500_files`        | ~380 ms est.  | Phase 1 baseline; bench was killed mid-run   |

**Observations:**

- `all_symbols + all_refs ≈ 348 ms` is paid by every read tool that races a
  watcher batch — the cache is cleared on each save, so the next tool call
  cold-loads from SQLite. This single cost dwarfs the < 100 ms p99 SLO from
  CLAUDE.md and is the largest leverage point in the Phase 3 cache rework.
- `delete_cascade_loop/100_files` at 76 ms ≈ 760 µs/file. Of that, the bulk
  is BEGIN/COMMIT + WAL fsync overhead — Phase 1 batching to a single
  transaction should compress this by ~10–50× depending on disk.

## Watcher save→query / 50-file burst (post-change)

Captured against `target/release/bench-watcher 50` after the Phase 1+2+3
changes landed. There's no paired pre-change number for this bin
because it was added alongside the fixes; the storage-bench cold-cache
cost above (`all_symbols + all_refs ≈ 348 ms`) is the proxy "before"
that the watcher → query loop would have paid synchronously on every
save.

```
── Single-file save → code_outline latency ─────────────
  code_outline    p50     0.24 ms   p95     0.38 ms   p99     0.46 ms   max     0.46 ms

── 50-file burst → indexed-confirm wall time ───────────
  filesystem writes:               0.83 ms
  watcher → queryable (50f):     311.61 ms
```

**Reading it:**

- `code_outline` p99 of **0.46 ms** is two orders of magnitude inside
  the < 100 ms SLO. The async refresh keeps reads off the cold-cache
  path entirely — they serve from the briefly-stale-but-warm snapshot
  while the background worker re-snapshots.
- The 50-file burst settling in **311 ms** end-to-end includes the
  250 ms watcher debounce window — that leaves ~60 ms for parallel
  parse + batched SQLite write + Tantivy commit on 50 files
  (~1.2 ms/file). Phase 2's `par_iter` swap is doing its job.
- Pre-change, the same workload would have been **debounce + parse +
  store + ~350 ms cold cache reload on the next read tool** — the new
  code amortizes the snapshot cost into a coalesced background thread.

## Reproduction

```sh
RECON_LICENSE_HMAC_KEY=bench-dev-only cargo build --release \
    -p recon-cli --bin bench-watcher
./target/release/bench-watcher 50          # or 100 for tighter percentiles
```

Lower `iterations` if you just want a smoke check (`bench-watcher 20`
runs in ~20 s); raise it for tighter percentiles.
