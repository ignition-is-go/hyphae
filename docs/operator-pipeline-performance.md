# Operator Pipeline Performance Proof

Hyphae 3.0 makes materialization explicit so an application can build one lazy
operator recipe and install one cached observation boundary. The release bar is
not a microbenchmark win: it must improve an application-shaped graph and must
demonstrably remove graph allocation without moving allocation into updates.

## Release thresholds

The operator-pipeline rewrite passes the performance gate when all of these are
true against `v2.0.1` on the same machine:

1. Deep reactive update latency improves by at least 15% at 16, 32, and 64
   join stages, with the Criterion confidence interval excluding zero.
2. At 80 logical operators (16 joins and four transforms per join), complete
   recipe construction plus graph installation performs at least 10% fewer
   allocations and allocates at least 35% fewer bytes.
3. A steady-state update performs no more allocations than `v2.0.1`.
4. Dropping the terminal materialized cell releases the installed candidate
   graph without requiring source destruction.
5. Default benchmarks remain bounded at depth 32. Depth 64 is isolated behind
   `deep-bench`, and allocation profiling compiles serially at a maximum depth
   of 16 join stages (80 logical operators).

The allocation thresholds are deliberately below the observed result so normal
allocator and platform variation does not make the gate machine-specific.
Allocation counts themselves are deterministic for a fixed allocator and
revision; elapsed nanoseconds from the allocation-counting harness are
diagnostic only. Criterion is the latency authority.

## Application-shaped latency

`hyphae/benches/reactive_graphs.rs` models one join per five operators, multiple
independently mutable `CellMap` sources, deep and wide updates, fan-out, and
materialization/teardown. The final join-runtime candidate measured against
`v2.0.1`:

| Join stages | `v2.0.1` | 3.0 candidate | Improvement |
| ---: | ---: | ---: | ---: |
| 16 | 232.75 us | 150.35 us | 35.4% |
| 32 | 545.71 us | 367.51 us | 32.7% |
| 64 | 1.4254 ms | 0.95739 ms | 32.8% |

The final pre-release run measured 95% confidence intervals of
`[148.89, 151.92] us`, `[362.13, 373.93] us`, and
`[941.30, 978.01] us` respectively. Criterion reported no statistically
significant change from the saved v3 candidate at depths 16 and 32.

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

The final release-candidate run on `f76d57d`, compared with the original
same-machine v2.0.1 measurements, produced:

| Operators | Setup allocations, v2 / v3 | Reduction | Allocated bytes, v2 / v3 | Reduction |
| ---: | ---: | ---: | ---: | ---: |
| 20 | 152 / 143 | 5.9% | 140,592 / 90,252 | 35.8% |
| 40 | 304 / 275 | 9.5% | 281,184 / 163,244 | 41.9% |
| 80 | 608 / 539 | 11.3% | 562,368 / 309,228 | 45.0% |

These setup totals include both lazy-recipe construction (`graph_build`) and
terminal installation (`materialize`). An earlier draft reported only the v3
materialization phase (473 calls at 80 operators) while labeling it total
setup; the final table corrects that accounting and uses 539 calls end to end.

Across every depth, the allocation count and byte count for 100 steady-state
updates were exactly equal between revisions. At 80 operators both performed
4,470 allocations totaling 114,272 bytes; those are transient signal-value
allocations, not graph construction deferred into the hot path.

At teardown, the candidate released all 307,692 net bytes retained by its
installed 80-operator graph. The `v2.0.1` terminal drop released only 35,052 of
560,832 retained bytes; the remaining intermediate graph storage was released
only when its sources died. This is the intended lifetime distinction of a
single explicit observation boundary.

## Conclusion

The result clears every release threshold. At representative depth, the rewrite
cuts update latency by roughly one third, complete setup allocation count by
11%, setup bytes and retained graph memory by roughly 45%, and does not increase
steady-state update allocation. The explicit `.materialize()` migration buys a
measured reduction in both work and resident graph state rather than merely
changing where that work occurs.
