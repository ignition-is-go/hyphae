# Phase 5 immutable four-join benchmark baseline

**Benchmark source checkpoint:** `afdda39` plus the benchmark-only change documented here  
**Date:** 2026-08-10  
**Decision:** **FAIL** — four workers do not meet the required 1.5x speedup.

## Frozen workload

`compiled_query/four_join_application/steady_batch/10000` is the pre-Phase-5 gate and must not be tuned to make a later implementation pass.

- 10,000 pre-existing application rows;
- four public `left_join_by -> map_joined_values` stages (the third join promotes the public two-join plan into a real `JoinRegion`, and the fourth extends it);
- 2,048 balanced relationship keys and four dimension matches per key per stage;
- four independently permuted foreign keys per application row;
- each stage folds right key/payload data into a typed row, records its stage bit and match count, and carries the generation;
- one untimed 10,000-row prewarm precedes the steady-state measurement;
- Criterion `iter_batched` constructs the input vector outside the timed routine; the timed routine includes `insert_many`, all synchronous propagation/publication, and an immediate output generation/stage-mask assertion;
- deterministic Fisher-Yates input order prevents accidentally sorted or shard-ordered publication from satisfying the correctness check.

The companion `rekey_10pct/10000` workload alternates 10% of relationship keys across every batch. A cold-promotion number is intentionally absent: the current arbitrary-N runtime has no promotion/sharded representation. The immutable gate measures warmed synchronous application work; Phase 5 promotion cost must be reported separately when such a transition exists.

## Correctness preflight

`four_join_application_preflight` uses the exact benchmark workload plus a standalone sequential evaluator. It checks:

- one batch callback containing exactly 10,000 updates;
- every old/new value and update ordinal, order-for-order;
- immediate settlement when `insert_many` returns;
- cardinality 10,000, generation 1, stage mask `0b1111`, and `[4, 4, 4, 4]` match counts;
- fixed canonical final-state digest `b394c0e6dc59b647f2dc1b05314b0043`;
- fixed order-sensitive trace digest `959cdbd3c843b5ae31f7575208113219`.

The application fold is commutative because initial `DashMap` snapshot iteration is unspecified; publication order remains strictly checked.

## Exact commands

```sh
HYPHAE_WORKER_THREADS=0 cargo bench -p hyphae --features scheduler \
  --bench compiled_map_queries -- \
  'compiled_query/four_join_application/steady_batch/10000' \
  --warm-up-time 2 --measurement-time 5 --sample-size 50 --noplot
HYPHAE_WORKER_THREADS=1 cargo bench -p hyphae --features scheduler \
  --bench compiled_map_queries -- \
  'compiled_query/four_join_application/steady_batch/10000' \
  --warm-up-time 2 --measurement-time 5 --sample-size 50 --noplot
HYPHAE_WORKER_THREADS=4 cargo bench -p hyphae --features scheduler \
  --bench compiled_map_queries -- \
  'compiled_query/four_join_application/steady_batch/10000' \
  --warm-up-time 2 --measurement-time 5 --sample-size 50 --noplot

HYPHAE_WORKER_THREADS={1,4} cargo bench -p hyphae --features scheduler \
  --bench compiled_map_queries -- \
  'compiled_query/four_join_application/rekey_10pct/10000' \
  --warm-up-time 2 --measurement-time 5 --sample-size 50 --noplot
```

Separate processes are mandatory because the Hyphae worker pool is a `LazyLock`.

## Results

| Workload | Workers | 95% interval | Midpoint |
|---|---:|---:|---:|
| steady 10k | 0 (pool disabled) | 20.409–22.533 ms | 21.415 ms |
| steady 10k | 1 | 18.285–19.321 ms | 18.815 ms |
| steady 10k | 4 | 18.180–19.358 ms | 18.765 ms |
| 10% rekey 10k | 1 | 20.447–21.504 ms | 20.965 ms |
| 10% rekey 10k | 4 | 19.481–20.580 ms | 20.016 ms |

Primary steady-state midpoint speedup is only **1.003x** (`18.815 / 18.765`). The conservative interval ratio is **0.945x** (`18.285 / 19.358`), far below 1.5x. Even comparing the noisier pool-disabled midpoint to four workers gives only **1.141x**. The 10%-rekey midpoint speedup is **1.047x**. Therefore the current arbitrary-N sequential runtime correctly establishes a failing baseline before any Phase 5 sharding work.

Raw outputs:

- `benchmark-results/phase5-preflight-four-join-workers-0.txt`
- `benchmark-results/phase5-preflight-four-join-workers-1.txt`
- `benchmark-results/phase5-preflight-four-join-workers-4.txt`
- `benchmark-results/phase5-preflight-four-join-rekey-workers-1.txt`
- `benchmark-results/phase5-preflight-four-join-rekey-workers-4.txt`
- `benchmark-results/phase5-preflight-full-lib-tests.txt`
- `benchmark-results/phase5-preflight-strict-clippy.txt`

Validation passed:

- `cargo test -p hyphae --features scheduler --test four_join_application_preflight`
- `cargo test -p hyphae --lib --all-features` — 358 passed
- `cargo clippy -p hyphae --all-targets --all-features -- -D warnings`

## Machine and toolchain

- AMD Ryzen 9 7900X3D, 12 physical cores / 24 hardware threads, one socket
- Linux `7.1.3-arch2-2`, x86_64
- rustc `1.97.1 (8bab26f4f 2026-07-14)`, LLVM 22.1.6
- cargo `1.97.1 (c980f4866 2026-06-30)`

The machine was not frequency-isolated; confidence intervals and raw outputs are retained. The gate is sufficiently far from 1.5x that this does not affect the fail decision.
