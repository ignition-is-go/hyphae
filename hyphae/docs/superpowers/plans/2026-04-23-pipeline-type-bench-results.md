# Pipeline Refactor: Bench Results

Last updated: 2026-04-25 (post lock-free Cell subscriber storage)

## Summary

The cumulative single-cell refactor — Pipeline trait + consume-self + lock-free
Cell subscriber storage + materialize no-op on Cell sources — collapses pure
operator chains to a single fused closure on the root cell, eliminates per-emit
`Vec` allocation in `Cell::notify`, and removes redundant ArcSwap stages at the
materialize boundary for plain Cell sources.

**Headline:** the single-cell baseline (one source, one subscriber, no chain)
dropped from **2.11 µs → 51.8 ns (40× speedup)**. Long pure-operator chains
went from O(N) propagation to roughly constant — depth-500 went from
**1.27 ms → 12.5 µs (102×)**. Construction cost (which was O(N²) at one point)
dropped 30-100× across all depths.

## pipeline_chains.rs deltas vs `pre-pipeline-refactor`

Mean estimates from criterion (100 samples, default config).

### Baseline (single source, single subscriber)

| label | pre | post | change | speedup |
|---|---:|---:|---:|---:|
| baseline_one_subscriber | 2.11 µs | 51.8 ns | -97.5% | **40×** |

The lock-free subscriber refactor (commit `8a16df0`) eliminated the per-emit
`Vec<(Uuid, Arc<dyn Fn>)>` allocation that `Cell::notify` previously did to
release the DashMap shard lock before invoking callbacks. That allocation
alone was the dominant cost at this scale.

### pure_chain_set (N chained `.map`s, terminal subscriber)

| depth | pre | post | change | speedup |
|---:|---:|---:|---:|---:|
| 1 | 4.36 µs | 111 ns | -97.5% | **39×** |
| 5 | 13.2 µs | 149 ns | -98.9% | **89×** |
| 25 | 59.5 µs | 421 ns | -99.3% | **141×** |
| 100 | 234 µs | 2.37 µs | -99.0% | **99×** |
| 250 | 606 µs | 6.24 µs | -99.0% | **97×** |
| 500 | 1270 µs | 12.5 µs | -99.0% | **102×** |

The chain depth scaling is nearly flat post-refactor: per-stage cost is the
fused closure body (~5-15 ns of arithmetic per stage). Pre-refactor each
stage paid an ArcSwap.store + DashMap iter + Vec alloc.

### mixed_chain_set (cycles of `.map.filter.map.tap`)

| cycles | pre | post | change | speedup |
|---:|---:|---:|---:|---:|
| 1 | 13.3 µs | 129 ns | -99.0% | **103×** |
| 5 | 49.7 µs | 211 ns | -99.6% | **236×** |
| 25 | 240 µs | 1.16 µs | -99.5% | **207×** |
| 100 | 988 µs | 5.30 µs | -99.5% | **186×** |

### chain_construction (build chain only, no propagation)

| depth | pre | post | change | speedup |
|---:|---:|---:|---:|---:|
| 10 | 43 µs | 1.43 µs | -96.7% | **30×** |
| 50 | 220 µs | 2.29 µs | -99.0% | **96×** |
| 250 | 1.14 ms | 36.2 µs | -96.9% | **31×** |
| 500 | 2.90 ms | 88.7 µs | -97.0% | **33×** |

Pre-refactor: each `.map` allocated a `Cell` + installed a subscription.
Mid-refactor (consuming `&self`): O(N²) recursive Arc clones during chain
construction. Final state (consuming `self`): O(N) closure boxing only.

### filter_blocking_chain_set

Pre-refactor: ~8.8 µs (blocked propagation halts at the first failing filter,
which still pays one `Cell::notify` cycle).
Post-refactor: **65.6 ns** (one `ArcSwap.store` on the root + closure that
short-circuits at the first failing predicate). Roughly **134×** speedup.

## What each step contributed

| commit | what changed | win |
|---|---|---|
| `0eee30d` … `dbd267b` | Pipeline trait + per-operator ports | Eliminated O(N) intermediate Cell allocations on chains |
| `46f27f6` | `Pipeline: !Clone` | Compile-time guard against silent work duplication |
| `6cbe978` | Consume `self` on operator methods | Eliminated O(N²) recursive Arc clone during construction |
| `93599ec` | `Cell::materialize` no-op | Killed redundant ArcSwap stage when source is already a Cell |
| `8a16df0` | Lock-free subscriber storage | Killed per-emit Vec alloc + DashMap shard locks in `Cell::notify` |

## Methodology

Hardware: AMD Ryzen 9 7900X3D. Sample count: criterion default (100 samples).
Pre-refactor baseline saved as `pre-pipeline-refactor`. Comparison via
`cargo bench --bench pipeline_chains -- --baseline pre-pipeline-refactor`.
