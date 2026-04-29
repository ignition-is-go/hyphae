# CellMap Query Plans: Bench Results

Last updated: 2026-04-25 (post lock-free Cell subscriber storage)

## Summary

The cumulative CellMap refactor — `MapQuery` plan trait + per-operator ports +
sink-driven runtime + lock-free Cell subscriber storage (which `CellMap`'s
internal `diffs_cell` and per-key cells benefit from) — collapses a chain of
N joins into one materialized output with one root subscription, and makes
each remaining emit cheaper by killing the per-emit `Vec` allocation in
`Cell::notify`.

**Headline:** depth-5 join chain went from **34.5 µs → 4.76 µs (7.2×)**.
Per-stage propagation cost fell from ~5.95 µs to ~0.64 µs — a **9× per-stage
reduction**. Even depth-1 (a single join, no chain) dropped from 10.7 µs to
2.21 µs (4.8×) — that win is purely from the lock-free subscriber storage in
the underlying Cell.

## join_chain_set deltas vs `pre-cellmap-query-plans`

Mean estimates from criterion (100 samples, default config).

| depth | pre (µs) | post (µs) | change   | speedup |
|------:|---------:|----------:|---------:|--------:|
| 1     | 10.7     | 2.21      | -79.4%   | **4.8×** |
| 2     | 16.4     | 2.83      | -82.3%   | **5.8×** |
| 3     | 22.3     | 3.51      | -84.3%   | **6.3×** |
| 5     | 34.5     | 4.76      | -85.0%   | **7.2×** |

Criterion change intervals (95% CI): all below -79%, p < 0.05.

## Observations

- **Per-stage propagation cost (slope of time vs depth)** dropped from
  ~5.95 µs/stage to ~0.64 µs/stage — a 9× per-stage reduction. Remaining
  per-stage cost is the in-fused-closure JoinState mutex acquire +
  HashMap lookups + closure dispatch. (Phase B index dedup would attack
  this further.)
- **Depth-1 cost (4.8×)** comes entirely from `Cell::notify` no longer
  allocating a per-emit `Vec<(Uuid, Arc<dyn Fn>)>` to release the
  DashMap shard lock — the lock-free `ArcSwap<Vec>` design lets notify
  load the subscriber list in one atomic and iterate without an alloc.
  This applies to every Cell in CellMap's machinery (`diffs_cell`, the
  per-key value cells), so the win compounds across reactive entry points.
- **Constant baseline ArcSwap.store**: the source `CellMap.insert` still
  performs one `ArcSwap.store` to update its diff cell value, regardless
  of chain depth. This is the irreducible per-emit cost.

## What each step contributed

| commit | what changed | win |
|---|---|---|
| `2d05751` … `38c8d95` | MapQuery + per-operator ports | Collapsed chain of N joins into one materialized output |
| `93599ec` | `CellMap::materialize` no-op | Killed redundant CellMap allocation when source is already a CellMap |
| `8a16df0` | Lock-free Cell subscriber storage | Killed per-emit Vec alloc inside CellMap's diffs_cell + per-key cells |

## Phase B candidates

The remaining ~0.64 µs/stage is dominated by:
- Per-join `JoinState` mutex acquire (currently a `std::sync::Mutex`)
- HashMap operations for index lookup / impacted-row tracking
- Per-output-diff closure dispatch through `MapDiffSink`

Index dedup (sharing one hash index across two joins keying on the same
field) would directly reduce mutex contention and HashMap work. Projection
pushdown (only carrying fields downstream readers actually project) would
cut diff payload sizes. Both deferred per the original phase-A scope.

## Methodology

Hardware: AMD Ryzen 9 7900X3D. Sample count: criterion default (100 samples).
Pre-refactor baseline saved as `pre-cellmap-query-plans`. Comparison via
`cargo bench --bench cell_map_chains -- --baseline pre-cellmap-query-plans`.
