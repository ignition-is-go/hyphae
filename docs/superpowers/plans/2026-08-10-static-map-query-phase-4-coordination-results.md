# Static MapQuery Phase 4 Coordination Checkpoint

**Date:** 2026-08-10

**Candidate:** `7580c27`

**Baseline:** Phase 2 compiler checkpoint (`489a3fd`)

## Change

Rust can now retain the concrete `LeftJoinPlan` and `MapValuesPlan` shapes
across the public extension methods. The compiler recognizes the common
`left_join_by -> map_values -> left_join_by -> map_values` region and installs
one coordinated physical runtime with:

- three direct root entry points;
- one plan-level synchronization boundary;
- both join states and their scratch in one transaction;
- direct propagation from the first join state into the second;
- no intermediate subscriber or callback boundary;
- output publication after releasing the plan lock.

The call syntax is unchanged. Named plan types are an intentional v3 API
tradeoff that preserves enough static structure for type-directed compilation.

## Environment and commands

```text
CPU: AMD Ryzen 9 7900X3D 12-Core Processor
Kernel: Linux 7.1.3-arch2-2 x86_64
rustc: 1.97.1 (8bab26f4f 2026-07-14)
cargo: 1.97.1 (c980f4866 2026-06-30)
```

```console
cargo bench -p hyphae --bench compiled_map_queries -- \
  'compiled_query/two_join_region/single'
cargo bench -p hyphae --bench compiled_map_queries -- \
  'compiled_query/two_join_region/batch'
tools/bench-map-query-allocations.sh
cargo test -p hyphae --lib traits::collections::left_join
cargo clippy -p hyphae --lib --all-features -- -D warnings
```

Criterion used its immediately preceding Phase 2 results as `base`; the
candidate was run on the same checkout, profile, toolchain, and machine.

## Latency

| Scenario | Candidate confidence interval | Change vs Phase 2 |
| --- | ---: | ---: |
| Two joins, one update | 3.4532-3.4696 us | **-8.9%** |
| Two joins, batch 1 | 3.4575-3.4734 us | **-7.4%** |
| Two joins, batch 10 | 8.9145-8.9658 us | **-26.5%** |
| Two joins, batch 100 | 63.141-63.707 us | **-37.1%** |
| Two joins, batch 1,000 | 573.81-578.70 us | **-51.2%** |

The sequential transaction crosses the 30% Phase 2-4 gate at batch 100 and
widens substantially with more useful work. Tiny updates improve without
paying a parallel-dispatch cost.

## Allocation profile

Raw results remain available from repository history at commit
`4465c6dbe364bfee9f97136d64420549af14d800`; current benchmark retention is
documented in [`docs/benchmarks/README.md`](../../benchmarks/README.md).

| Two-join phase | Phase 2 | Candidate | Change |
| --- | ---: | ---: | ---: |
| Materialize allocation calls | about 14,090 | 9,037 | about **-35.9%** |
| Materialize retained bytes | not separately archived | 755,732 | - |
| 100 single-update allocation calls | not separately archived | 1,400 | - |
| One 100-row batch allocation calls | not separately archived | 536 | - |
| Teardown net bytes | not separately archived | -977,972 | - |

The remaining steady-state allocations include materializing `Vec<RV>` for
the public joined tuple before each `map_values` closure and transient impacted
row buffers. A join-aware projection node is the next allocation target.

## Compile resources

The representative benchmark was built in a fresh target directory with one
Cargo build job.

| Metric | Phase 2 | Candidate | Change |
| --- | ---: | ---: | ---: |
| Wall time | 46.87 s | 48.69 s | +3.9% |
| User CPU | 43.95 s | 44.71 s | +1.7% |
| System CPU | 3.21 s | 4.25 s | +32.4% |
| Peak RSS | 445,316 KiB | unavailable (`/usr/bin/time` absent) | - |
| Benchmark executable | 4,564,768 bytes | 4,513,608 bytes | -1.1% |

The normal runtime tests and strict all-feature library Clippy pass. The
workspace `--all-targets --all-features` command was also attempted with one
build job, but the unrelated feature-gated `reactive_graphs_64` scalar deep
benchmark was killed by the host for memory exhaustion. This is a compile
resource failure, not a test assertion failure, and is recorded rather than
reported as a passing gate.

## Correctness counters

The candidate and baseline materialize 1,000 output rows in the benchmark.
Dedicated incremental tests exercise changes from the left root and both right
roots, removals from every root, and batched updates. All 13 left-join tests
pass, including the two new coordinated-region tests.

## Decision

Keep the coordinated physical runtime. It proves that direct roots and one
plan transaction materially improve the join batch curve. This is a Phase 4
checkpoint, not completion: longer chains, repeated relationship-index reuse,
join-aware projection fusion, deterministic sharding, and collision-error
semantics remain required by the approved design.
