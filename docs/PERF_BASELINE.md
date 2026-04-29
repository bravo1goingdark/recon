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

## Watcher save→query / 50-file burst

`bench-watcher` bin compiled at this point (release build pending tantivy
crate compilation). Will run a single baseline pass once it lands. Numbers
will be appended below before the post-change re-run.

```
TODO baseline (single iteration):
  code_outline    p50 ___ ms   p95 ___ ms   p99 ___ ms
  watcher → queryable (50f):  ___ ms
```

## Post-change results

Filled in after the implementation phases land. Same bench commands, same
flags, same repo state.
