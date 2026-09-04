# Static MapQuery Phase 4 Joined Projection Results

**Date:** 2026-08-10

**Candidate:** `d431be2`

**Baseline:** coordinated two-join checkpoint (`7580c27`)

## Change

`LeftJoinPlan` and coordinated `TwoLeftJoinPlan` now expose
`map_joined_values`. Its projection receives the left value and the join
runtime's indexed `(right_key, right_value)` slice directly. This removes the
otherwise mandatory clone/collection of an intermediate `(left, Vec<right>)`
when callers immediately project a join into another value.

The compiler carries tuple-based `map_values` and direct
`map_joined_values` through a sealed, statically dispatched projection type.
Both surfaces continue to select the coordinated three-root runtime. The
benchmark and allocation adapters changed only from destructuring a cloned
joined tuple to folding the equivalent indexed match slice.

## Validation

```console
cargo test -p hyphae --lib traits::collections::left_join
cargo clippy -p hyphae --all-targets --all-features -- -D warnings
cargo bench -p hyphae --bench compiled_map_queries -- \
  'compiled_query/two_join_region/single'
cargo bench -p hyphae --bench compiled_map_queries -- \
  'compiled_query/two_join_region/batch'
cargo bench -p hyphae --bench compiled_map_queries -- \
  'compiled_query/repeated_relation_four_join/single'
tools/bench-map-query-allocations.sh
```

All tests and strict Clippy checks pass.

## Latency

| Scenario | Candidate confidence interval | Change vs coordinated baseline |
| --- | ---: | ---: |
| Two joins, one update | 3.3398-3.3530 us | **-3.3%** |
| Two joins, batch 1 | 3.3742-3.3886 us | **-2.7%** |
| Two joins, batch 10 | 8.0928-8.1381 us | **-9.1%** |
| Two joins, batch 100 | 53.013-53.359 us | **-15.6%** |
| Two joins, batch 1,000 | 571.51-577.42 us | -0.8%, within noise |
| Four repeated-relation joins, one update | 3.8349-3.8490 us | **-16.8%** |

Relative to the Phase 2 compiler checkpoint, the cumulative two-join
improvement is approximately 12% for one-row updates, 47% for batch 100, and
40% for batch 1,000. The four-join workload is approximately 17% faster than
the first coordinated checkpoint even though only its first two joins share a
state transaction today.

## Allocation profile

Raw measurements remain available from repository history at commit
`4465c6dbe364bfee9f97136d64420549af14d800`; current benchmark retention is
documented in [`docs/benchmarks/README.md`](../../benchmarks/README.md).

| Scenario and phase | Coordinated baseline | Candidate | Change |
| --- | ---: | ---: | ---: |
| Two joins, materialize calls | 9,037 | 7,039 | **-22.1%** |
| Two joins, 100 single updates | 1,400 | 1,201 | **-14.2%** |
| Two joins, batch 100 | 536 | 337 | **-37.1%** |
| Two joins, retained materialized bytes | 755,732 | 755,596 | flat |
| Four joins, materialize calls | 21,619 | 17,616 | **-18.5%** |
| Four joins, 100 single updates | 2,000 | 1,600 | **-20.0%** |
| Four joins, batch 100 | 944 | 548 | **-41.9%** |
| Four joins, retained materialized bytes | 1,325,176 | 1,325,176 | flat |

The eliminated allocations were transient joined-value vectors, so retained
state is correctly unchanged. The remaining large-batch time is dominated by
join index/state work rather than the projection representation.

## Decision

Keep `map_joined_values` as the specific high-performance projection surface.
It improves every affected steady-state allocation count and materially
improves the four-join application shape. The next physical work should make
longer join chains one coordinated typed runtime and reuse repeated relation
indexes; the flat 1,000-row incremental result also reinforces that Rayon must
parallelize that whole stateful region rather than individual projections.
