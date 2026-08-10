# Static MapQuery Phase 5 Determinism and Parallel Groundwork

**Date:** 2026-08-10

**Candidate:** `c10bdcc`

**Baseline:** joined-projection checkpoint (`d431be2`, documented at
`47562da`)

## Change

Join buckets and incremental work lists now preserve insertion/input order
explicitly. Stable order no longer depends on `FxHashSet` iteration. The same
representation removes hash-set churn from the common join path.

With the `scheduler` feature enabled on native targets, keyed join projection
work above an estimated 65,536 units executes on Hyphae's shared dedicated
Rayon pool. A 49,152-unit exit threshold provides hysteresis. Results are
collected through Rayon's indexed iterator and committed sequentially in input
ordinal order before the initiating source mutation returns. Smaller work
stays on the calling thread; wasm and builds without `scheduler` remain
sequential.

Rekeying runtimes now also preserve source/emission order and enforce the
documented unique-output-key contract. Duplicate keys fail in release builds
instead of silently overwriting an unrelated row. Atomic output-key swaps in
one batch remain valid.

## Validation

```console
cargo test -p hyphae --lib --all-features
cargo clippy -p hyphae --all-targets --all-features -- -D warnings
cargo bench -p hyphae --bench compiled_map_queries -- \
  --save-baseline phase5-deterministic
HYPHAE_WORKER_THREADS=0 cargo bench -p hyphae --features scheduler \
  --bench compiled_map_queries -- --save-baseline phase5-sequential-10k \
  'compiled_query/two_join_region/batch/10000'
HYPHAE_WORKER_THREADS=4 cargo bench -p hyphae --features scheduler \
  --bench compiled_map_queries -- --baseline phase5-sequential-10k \
  'compiled_query/two_join_region/batch/10000'
tools/bench-map-query-allocations.sh
```

All 319 library tests pass with all features. The new tests prove right-match
order, right-triggered left publication order, synchronous settlement of a
10,000-row parallel batch, exact input-order publication, collision rejection,
and atomic rekey swaps.

## Sequential latency

| Scenario | Joined-projection baseline | Candidate | Change |
| --- | ---: | ---: | ---: |
| Two joins, one update | 3.3398-3.3530 us | 3.3590-3.3738 us | +0.6% |
| Four joins, one update | 3.8349-3.8490 us | 3.7842-3.8018 us | **-1.3%** |
| Two joins, batch 10 | 8.0928-8.1381 us | 7.2256-7.2700 us | **-10.7%** |
| Two joins, batch 100 | 53.013-53.359 us | 48.615-48.913 us | **-8.3%** |
| Two joins, batch 1,000 | 571.51-577.42 us | 441.28-444.71 us | **-22.8%** |
| Rekey between joins, one update | Phase 2: 3.8789-3.8937 us | 3.9880-4.0050 us | +2.9% |

Stable vector buckets are substantially faster at batch scale. Collision
ownership and atomic rekey validation cost about 3% in the rekey benchmark;
that regression is accepted for correct unique-key semantics and remains
within the 5% tiny-update gate.

## Parallel crossover

| Workload | Forced sequential | Four workers | Change |
| --- | ---: | ---: | ---: |
| Two joins, batch 1,000 | 434.50-436.92 us | 439.39-443.62 us | +1.1%, sequential selected after final threshold tuning |
| Two joins, batch 10,000 | 4.6300-4.7095 ms | 4.2668-4.4481 ms | **-6.7%** |

The 10,000-row result proves useful deterministic multicore execution, but it
does **not** satisfy the Phase 5 1.5x gate. Only pure projection work is
parallel in this checkpoint; index mutation and ordered commit remain serial.
This path is retained only above the measured crossover. Exclusive shard-owned
state is still required for the intended scaling.

## Allocation profile

Raw measurements are archived at
`benchmark-results/map-query-allocations/20260810T113240Z/`.

| Scenario and phase | Joined projection | Candidate | Change |
| --- | ---: | ---: | ---: |
| Two joins, materialize calls | 7,039 | 6,843 | **-2.8%** |
| Two joins, retained bytes | 755,596 | 707,604 | **-6.4%** |
| Two joins, 100 single updates | 1,201 | 1,401 | +16.7% |
| Two joins, batch 100 | 337 | 344 | +2.1% |
| Four joins, materialize calls | 17,616 | 17,251 | **-2.1%** |
| Four joins, retained bytes | 1,325,176 | 1,227,116 | **-7.4%** |

Ordered work tracking trades some tiny-update allocation calls for lower
retained join state and much lower batch latency. Reusable inline/small work
storage remains an optimization opportunity.

## Decision

Keep the deterministic ordered representation, collision enforcement, and the
high-threshold shared-pool path. Do not declare Phase 5 complete: the measured
parallel gain is real but well short of the scaling gate. The next iteration
must partition mutation, indexes, projection, and cache work into exclusive
plan shards, then perform one stable merge/commit.
