# Static MapQuery Phase 2 Results

**Date:** 2026-08-10

**Runtime commit:** `3ad490b`

**Baseline:** Phase 1 (`b496769`)

## Change

Every ordinary plan edge now carries a concrete sink type through the sealed
query compiler. `Arc<dyn Fn(&MapDiff)>` is gone between projections, joins,
grouping stages, and the materialization boundary. Type erasure remains only
at root subscriber registries and the explicit `.share()` multicast boundary.
Two-root joins share concrete sink/state handles without erasing either entry
point.

The scheduler's dedicated Rayon pool was also moved behind one crate-private
executor so the later query sharding phase can reuse it without creating a
second pool. Query execution remains sequential without the scheduler feature.

## Validation

```console
cargo test -p hyphae --lib
cargo test -p hyphae --tests --all-features
cargo clippy -p hyphae --all-targets --all-features -- -D warnings
cargo bench -p hyphae --bench compiled_map_queries -- --noplot
tools/bench-map-query-allocations.sh
```

All checks passed, including scheduler contention tests.

## Latency

| Scenario | Phase 1 | Phase 2 | Change |
| --- | ---: | ---: | ---: |
| Four-stage projection, one update | 2.5892–2.6042 µs | 2.5417–2.5558 µs | **-1.7%** |
| Two joins, one update | 3.9055–3.9171 µs | 3.7488–3.7666 µs | **-3.9%** |
| Four repeated-relation joins, one update | 4.7694–4.8057 µs | 4.6190–4.6390 µs | **-3.3%** |
| Rekey between joins, one update | 4.0178–4.0354 µs | 3.8789–3.8937 µs | **-3.5%** |
| Two-join batch, 100 rows | 99.717–100.48 µs | 98.373–99.223 µs | **-1.3%** |
| Two-join batch, 1,000 rows | 0.95948–0.96628 ms | 0.95853–0.96948 ms | no significant change |

The single-row improvements confirm LLVM can inline across the concrete plan
edges. Large batches remain dominated by intermediate batch construction and
stateful join work, not dispatch.

## Allocation profile

Raw measurements are archived at
`benchmark-results/map-query-allocations/20260810T103115Z/`.

Allocation counts and retained bytes are effectively unchanged from Phase 1,
as expected: this phase removes virtual calls and boxed closure allocations,
while the large retained maps are join state. Projection materialization moved
from 1,508 to 1,505 allocation calls; two-join materialization remained about
14,090 calls; repeated four-join materialization remained about 35,750 calls.

## Compile resources

| Metric | Phase 1 | Phase 2 | Change |
| --- | ---: | ---: | ---: |
| Wall time | 44.87 s | 46.87 s | +4.5% |
| User CPU | 42.15 s | 43.95 s | +4.3% |
| System CPU | 3.04 s | 3.21 s | +5.6% |
| Peak RSS | 493,488 KiB | 445,316 KiB | -9.8% |
| Benchmark executable | 4,309,520 bytes | 4,564,768 bytes | +5.9% |

The larger executable is the expected cost of monomorphizing hot plan edges.

## Parallel threshold experiment

A deliberately isolated experiment dispatched every key-preserving stage for
batches of at least 256 changes onto the shared pool. It was removed after the
1,000-row two-join benchmark regressed by roughly 22%. This validates the
physical-plan rule: Rayon must wrap a whole compiled region with shard-owned
state and one ordered commit barrier, never individual semantic operators.

## Decision

Keep the monomorphized compiler and shared executor. The next phase must remove
the independent join-state chain and compile root entry points into coordinated
state. Parallel execution should be added only after that state can be split
into exclusive shards and committed in stable ordinal order.
