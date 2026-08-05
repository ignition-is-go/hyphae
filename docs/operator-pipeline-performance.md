# Operator Pipeline Performance Proof

Hyphae 3.0 makes materialization explicit so an application can build one lazy
operator recipe and install one cached observation boundary. The release bar is
not a microbenchmark win: it must improve an application-shaped graph without
moving allocation into steady-state updates or retaining graph state after the
terminal observation is dropped.

## Release thresholds

The operator-pipeline rewrite passes the performance gate when all of these are
true against `v2.0.1` on the same machine:

1. Deep reactive update latency improves by at least 20% at 16, 32, and 64
   join stages, using the upper bound of the candidate's Criterion confidence
   interval against the same-machine `v2.0.1` measurement.
2. At 80 logical operators (16 joins and four transforms per join), complete
   recipe construction plus graph installation allocates no more than 5% more
   bytes than `v2.0.1`. Allocation-call count is reported as diagnostic data.
3. A steady-state update performs no more allocations than `v2.0.1`.
4. Dropping the terminal materialized cell releases the installed candidate
   graph without requiring source destruction.
5. Default benchmarks remain bounded at depth 32. Depth 64 is isolated behind
   `deep-bench`, and allocation profiling compiles serially at a maximum depth
   of 16 join stages (80 logical operators).

Allocation counts themselves are deterministic for a fixed allocator and
revision; elapsed nanoseconds from the allocation-counting harness are
diagnostic only. Criterion is the latency authority. Setup bytes are a
non-regression bound rather than the primary win: the public v3 API requires a
materialized seed at each join, so an application with one join per five
operators still installs one stateful cell per join.

## Application-shaped latency

`hyphae/benches/reactive_graphs.rs` models one join per five operators, multiple
independently mutable `CellMap` sources, deep and wide updates, fan-out, and
materialization/teardown. The final join-runtime candidate measured against
`v2.0.1`:

| Join stages | `v2.0.1` | 3.0 candidate | Improvement |
| ---: | ---: | ---: | ---: |
| 16 | 232.75 us | 154.24 us | 33.7% |
| 32 | 545.71 us | 385.04 us | 29.4% |
| 64 | 1.4254 ms | 1.0506 ms | 26.3% |

The final pre-release run measured 95% confidence intervals of
`[153.25, 155.26] us`, `[383.10, 387.01] us`, and
`[1.0415, 1.0618] ms` respectively. Even the slow end of each interval clears
the 20% release threshold against the same-machine v2 result.

These depths represent approximately 80, 160, and 320 logical operators at the
configured join density. The opt-in depth-64 target must be run alone because
the static type itself can require multiple gigabytes of compiler memory.

## Allocation and materialization result

`tools/operator_allocation_profile.rs` wraps the system allocator and measures
graph construction, the terminal materialization, 100 source updates, and
terminal-cell teardown separately. `tools/bench-operator-allocations.sh`
archives one selected v3 revision into a temporary directory and builds it with
one compiler job. This keeps the release benchmark free of legacy conditional
code while preserving a reproducible allocation snapshot for every 3.x
revision.

Run the exact comparison with:

```console
REVISION=HEAD tools/bench-operator-allocations.sh
```

The final release-candidate run on `209eca8`, compared with the original
same-machine v2.0.1 measurements, produced:

| Operators | Setup allocations, v2 / v3 | Change | Allocated bytes, v2 / v3 | Change |
| ---: | ---: | ---: | ---: | ---: |
| 20 | 152 / 176 | +15.8% | 140,592 / 142,032 | +1.0% |
| 40 | 304 / 351 | +15.5% | 281,184 / 283,788 | +0.9% |
| 80 | 608 / 704 | +15.8% | 562,368 / 568,128 | +1.0% |

These setup totals include both recipe construction and the explicit
materialization required before every join, plus terminal installation. The
previous candidate harness chained opaque joins internally and therefore did
not represent compilable external v3 code; its 539-call / 309,228-byte result
is superseded by this public-API-shaped measurement.

Across every depth, the allocation count and byte count for 100 steady-state
updates were exactly equal between revisions. At 80 operators both performed
4,470 allocations totaling 114,272 bytes; those are transient signal-value
allocations, not graph construction deferred into the hot path.

At teardown, the candidate released all 566,592 net bytes retained by its
installed 80-operator graph. The `v2.0.1` terminal drop released only 35,052 of
560,832 retained bytes; the remaining intermediate graph storage was released
only when its sources died. Explicit join boundaries therefore cost roughly
the same retained bytes during observation, but v3 gives the terminal graph a
complete and predictable lifetime.

## Conclusion

The result clears every release threshold. Across 80–320 logical operators,
the rewrite cuts update latency by 26–34%, does not increase steady-state
update allocation, holds setup bytes within 1% of v2, and releases the entire
installed graph at the terminal boundary. Setup allocation calls are currently
about 16% higher; that is the clearest next optimization target and can be
improved behind the opaque `Materialize` API without another consumer-facing
break.
