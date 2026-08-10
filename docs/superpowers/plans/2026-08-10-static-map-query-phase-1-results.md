# Static MapQuery Phase 1 Results

**Date:** 2026-08-10

**Runtime commit:** `b496769`

**Baseline:** `7fcb142` / `2026-08-10-static-map-query-phase-0-baseline.md`

**Scope:** Associated query types, semantic projection operators, stateless
key-preserving diff transforms, and relationship-typed foreign keys.

## Implementation

Phase 1 changes the public query contract from `MapQuery<K, V>` to
`MapQuery<Key = K, Value = V>` and adds the semantic operators `select_by`,
`map_values`, `filter_map_values`, `map_entries`, `filter_map_entries`, and
`flat_map_entries`.

`select`, `select_by`, `map_values`, and `filter_map_values` now transform
`MapDiff` values directly. They no longer mirror the complete source map or
maintain per-source output-key and output-value hash tables. Rekeying operators
remain on the general stateful runtime until the compiled physical-plan phase.

Foreign-key joins now take a zero-sized `ForeignKeyRelation` marker. The
relationship is independent of the current parent payload type, so a
key-preserving projection between joins retains the proof required by a later
foreign-key join.

## Validation

```console
cargo test --workspace --all-targets --all-features
cargo clippy -p hyphae --all-targets --all-features -- -D warnings
cargo bench -p hyphae --bench compiled_map_queries -- --noplot
tools/bench-map-query-allocations.sh
```

All tests, all-target builds, and Clippy passed. The all-features run includes
the scheduler concurrency and prolonged-contention suites.

## Latency

All intervals are Criterion 95% confidence intervals. The change column uses
interval midpoints; negative is faster.

| Scenario | Before | Phase 1 | Change |
| --- | ---: | ---: | ---: |
| Four-stage key-preserving projection, one update | 3.5343–3.5533 µs | 2.5892–2.6042 µs | **-26.7%** |
| Two joins separated by projection, one update | 4.1174–4.1352 µs | 3.9055–3.9171 µs | **-5.2%** |
| Four joins over one repeated relation, one update | 5.3422–5.3721 µs | 4.7694–4.8057 µs | **-10.6%** |
| Rekey between two joins, one update | 4.1357–4.1581 µs | 4.0178–4.0354 µs | **-2.9%** |
| Two-join batch, 1 row | 4.1573–4.1744 µs | 3.7815–3.7989 µs | **-9.0%** |
| Two-join batch, 10 rows | 15.731–15.813 µs | 12.190–12.276 µs | **-22.4%** |
| Two-join batch, 100 rows | 133.38–134.37 µs | 99.717–100.48 µs | **-25.2%** |
| Two-join batch, 1,000 rows | 1.2220–1.2350 ms | 0.95948–0.96628 ms | **-21.6%** |

The join scenarios improve because each semantic projection between join
stages no longer creates another mirrored map runtime. Join indexes themselves
are still independent and stateful; their compilation and reuse are Phase 2+
work.

## Allocation and retention

Raw output and the exact harness are archived under
`benchmark-results/map-query-allocations/20260810T101525Z/`.

### Four-stage projection region

| Phase | Before alloc calls | Phase 1 alloc calls | Before net bytes | Phase 1 net bytes |
| --- | ---: | ---: | ---: | ---: |
| Build | 4 | 0 | 64 | 0 |
| Materialize | 7,557 | 1,508 | 585,468 | 127,384 |
| 100 single updates | 3,000 | 900 | -15,888 | -7,920 |
| One 100-row batch | 865 | 348 | 12,252 | 13,100 |
| Teardown | 0 | 0 | -574,152 | -124,004 |

Materialization uses **80.0% fewer allocation calls** and retains **78.2%
fewer bytes**. Single-row propagation uses **70.0% fewer allocation calls**.
The slightly higher post-batch net byte snapshot is allocator timing; teardown
returns the remaining projection state and the retained footprint is much
smaller overall.

### Two-join region

| Phase | Before alloc calls | Phase 1 alloc calls | Before net bytes | Phase 1 net bytes |
| --- | ---: | ---: | ---: | ---: |
| Build | 2 | 0 | 32 | 0 |
| Materialize | 28,212 | 14,091 | 1,865,748 | 1,104,772 |
| 100 single updates | 2,800 | 2,000 | -71,888 | -71,888 |
| One 100-row batch | 1,556 | 944 | 15,852 | 16,056 |
| Teardown | 0 | 0 | -2,087,000 | -1,326,536 |

Materialization uses **50.1% fewer allocation calls** and retains **40.8%
fewer bytes**. Single updates use **28.6% fewer allocation calls**, and the
100-row batch uses **39.3% fewer**.

### Four joins over one repeated logical relation

| Phase | Before alloc calls | Phase 1 alloc calls | Before net bytes | Phase 1 net bytes |
| --- | ---: | ---: | ---: | ---: |
| Build | 4 | 0 | 64 | 0 |
| Materialize | 72,028 | 35,751 | 3,543,272 | 2,021,864 |
| 100 single updates | 4,800 | 3,200 | -71,888 | -71,888 |
| One 100-row batch | 2,995 | 1,763 | 16,396 | 15,172 |
| Teardown | 0 | 0 | -3,622,260 | -2,100,176 |

Materialization uses **50.4% fewer allocation calls** and retains **42.9%
fewer bytes**. Single updates use **33.3% fewer allocation calls**, and the
100-row batch uses **41.1% fewer**. The remaining approximately 2 MiB is the
direct target for coordinated multi-join state and relationship-index reuse.

## Compile resources

The dedicated benchmark was built in a fresh isolated target directory with
one build job.

| Metric | Before | Phase 1 | Change |
| --- | ---: | ---: | ---: |
| Wall time | 46.83 s | 44.87 s | -4.2% |
| User CPU | 42.78 s | 42.15 s | -1.5% |
| System CPU | 4.37 s | 3.04 s | -30.4% |
| Peak RSS | 417,160 KiB | 493,488 KiB | +18.3% |
| Benchmark executable | 4,338,864 bytes | 4,309,520 bytes | -0.7% |

The increased compiler memory is within the design's explicit tradeoff:
compile resources may grow in exchange for faster, more specialized runtime
code. Runtime latency, allocation count, retained memory, and executable size
all moved in the intended direction.

## Phase 1 decision

The semantic API is validated and the type information is producing immediate
runtime wins. Phase 2 should replace recursive boxed sink installation with a
monomorphized compiled runtime and fuse adjacent stateless operators into one
diff pass. The repeated-relation benchmark remains the primary guardrail for
the subsequent coordinated-index phase.
