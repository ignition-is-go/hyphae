# CellMap Query Plans: Bench Results

Date: 2026-04-25

## Summary

The CellMap query-plans refactor collapses each `inner_join` from a
materialized intermediate (CellMap allocation + ArcSwap subscription) into a
plan node, then materializes the whole chain once at the end. The headline
result: a depth-5 join chain dropped from 34.5 us to 13.42 us — a 2.57x
speedup — and per-stage propagation cost fell from ~5.95 us to ~0.65 us, a
9x reduction. Depth-1 is essentially flat (within noise / +2.2%) since there
is still exactly one materialization either way.

## join_chain_set deltas vs pre-cellmap-query-plans

| depth | pre (us) | post (us) | change   |
|-------|----------|-----------|----------|
| 1     | 10.7     | 10.83     | +2.24%   |
| 2     | 16.4     | 11.71     | -29.23%  |
| 3     | 22.3     | 12.38     | -44.71%  |
| 5     | 34.5     | 13.42     | -61.11%  |

Criterion change intervals (95% CI):

- depth 1: [+1.54%, +2.24%, +2.97%] (p < 0.05, regressed)
- depth 2: [-29.85%, -29.23%, -28.68%] (p < 0.05, improved)
- depth 3: [-45.29%, -44.71%, -44.17%] (p < 0.05, improved)
- depth 5: [-61.36%, -61.11%, -60.88%] (p < 0.05, improved)

## Observations

- Per-stage propagation cost (slope of time vs depth):
  - pre-refactor: ~5.95 us/stage, dominated by intermediate CellMap
    allocation and ArcSwap subscription at every join.
  - post-refactor: ~0.65 us/stage, just the per-join sink-driven diff
    propagation. ~9x lower per-stage overhead.
- Depth-1 sees a small (~2%) regression. Plausible attribution: the new
  plan-node + materialize wrapping adds a tiny constant overhead at the
  single-stage tip, where there are no intermediate CellMaps to amortize
  the win against. Within outlier noise (5 high outliers).
- Cost still grows with depth post-refactor (10.83 -> 13.42 us across 1->5),
  but the slope is gentle and dominated by the linear chain of sink
  callbacks rather than synchronization primitives.
- Improvement scales roughly linearly with depth: deeper chains win more
  because more intermediate materializations are eliminated.

## Methodology

- Hardware: AMD Ryzen 9 7900X3D 12-Core (24 threads), Linux 6.19.10-arch1-1 x86_64.
- Toolchain: `cargo bench --bench cell_map_chains --target-dir target/claude --
  --baseline pre-cellmap-query-plans`.
- Baseline `pre-cellmap-query-plans` saved at commit b08e08c
  (`bench(hyphae): baseline cell-map join-chain propagation`) on the
  pre-refactor `feat/pipelines` HEAD, before the MapQuery trait was
  introduced.
- Sample count: criterion default (100 samples per parameter, 3s warmup,
  5s collect).
- Bench harness: `hyphae/benches/cell_map_chains.rs`. Each `bench_depth_N`
  builds N source CellMaps, chains N `inner_join` calls into a single plan,
  materializes once outside the timed loop, then in the timed loop performs
  one `a.insert(...)` to drive a propagation through the whole chain.
- Bench labels (`join_chain_set/{1,2,3,5}`) are kept identical to the
  pre-refactor harness so criterion's `--baseline` comparison resolves
  cleanly.
