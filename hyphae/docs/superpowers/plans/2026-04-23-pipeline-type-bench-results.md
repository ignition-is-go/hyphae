# Pipeline Refactor: Bench Results

Date: 2026-04-24

## Summary

The Pipeline refactor collapses chains of pure operators (`map` / `filter` /
`tap`) into a single fused closure installed on the root `Cell`. Pre-refactor
each operator allocated its own `Cell` (with `ArcSwap` store + subscriber
table), so propagation cost grew linearly with chain depth. Post-refactor the
per-set propagation cost is essentially independent of pure-chain depth — at
depth 500 the chain runs in ~18 µs vs ~1.26 ms before, a 71x improvement, and
the constant baseline (one cell, one subscriber, ~2.1 µs) is unchanged. Filter
chains gain similarly: a blocked emission now exits at the root (one ArcSwap
write + one closure call) regardless of nominal chain length.

## pipeline_chains.rs deltas vs `pre-pipeline-refactor`

Times are mean estimates from criterion (100 samples, default config).

### baseline (single source, single subscriber)

| label                     | pre        | post       | change     | speedup |
|---------------------------|-----------:|-----------:|-----------:|--------:|
| baseline_one_subscriber   |   2.11 µs  |   2.13 µs  |   +0.9 %   |  1.0x   |

Within noise — confirms the runtime overhead unrelated to chains is unchanged.

### pure_chain_set (N `.map`s, then subscribe)

| label                | pre        | post       | change      | speedup |
|----------------------|-----------:|-----------:|------------:|--------:|
| pure_chain_set/1     |   4.37 µs  |   4.38 µs  |   +0.2 %    |   1.0x  |
| pure_chain_set/5     |  13.45 µs  |   4.50 µs  |  -66.6 %    |   3.0x  |
| pure_chain_set/25    |  59.50 µs  |   4.87 µs  |  -91.8 %    |  12.2x  |
| pure_chain_set/100   | 232.41 µs  |   7.01 µs  |  -97.0 %    |  33.2x  |
| pure_chain_set/250   | 599.96 µs  |  11.62 µs  |  -98.1 %    |  51.6x  |
| pure_chain_set/500   |   1.26 ms  |  17.75 µs  |  -98.6 %    |  71.0x  |

Pre-refactor cost per stage was ~2.5 µs (one `ArcSwap::store` + one subscriber
notify per intermediate cell). Post-refactor adding a stage adds only the
closure call itself (a few ns each); per-set cost stays close to the
single-cell baseline plus a small slope from inlined arithmetic.

### mixed_chain_set (cycles of `map.filter.map.tap`)

| label                | pre        | post       | change      | speedup |
|----------------------|-----------:|-----------:|------------:|--------:|
| mixed_chain_set/1    |  13.28 µs  |   4.53 µs  |  -65.9 %    |   2.9x  |
| mixed_chain_set/5    |  49.69 µs  |   4.65 µs  |  -90.6 %    |  10.7x  |
| mixed_chain_set/25   | 240.45 µs  |   5.85 µs  |  -97.6 %    |  41.1x  |
| mixed_chain_set/100  | 976.04 µs  |  10.26 µs  |  -98.9 %    |  95.1x  |

Same story as `pure_chain_set` but with mixed pure operators in each cycle.
At 100 cycles (401 ops total) the fused chain is 95x faster than the
intermediate-cell version.

### chain_construction (build N `.map`s, no subscribe)

| label                   | pre        | post       | change     | speedup |
|-------------------------|-----------:|-----------:|-----------:|--------:|
| chain_construction/10   |  43.34 µs  |   3.45 µs  |  -92.1 %   |  12.6x  |
| chain_construction/50   | 222.06 µs  |   4.40 µs  |  -98.0 %   |  50.4x  |
| chain_construction/250  |   1.15 ms  |  39.40 µs  |  -96.6 %   |  29.2x  |
| chain_construction/500  |   2.90 ms  |  91.69 µs  |  -96.8 %   |  31.6x  |

Construction was originally only 8 % faster at depth 500 because the
`&self`-receiver clone-on-map pattern made chain construction O(N²) in Arc
bumps: each `MapPipeline::clone()` recursively cloned its `source`, so depth
N built `1 + 2 + ... + N` source clones during chain construction. Switching
to consuming `self` gives O(N) construction — only the closure allocation per
stage. Pure-pipeline construction now allocates only `Arc<closure>` per stage
(no recursive source clone), so cost scales linearly with depth and is
~30x faster at 500 stages than the &self version was.

### filter_blocking_chain_set (alternating `map.filter`, predicate blocks half)

| label                            | pre        | post       | change    | speedup |
|----------------------------------|-----------:|-----------:|----------:|--------:|
| filter_blocking_chain_set/5      |   8.88 µs  |   2.25 µs  |  -74.7 %  |   4.0x  |
| filter_blocking_chain_set/25     |   8.81 µs  |   2.20 µs  |  -75.0 %  |   4.0x  |
| filter_blocking_chain_set/100    |   8.87 µs  |   2.23 µs  |  -74.9 %  |   4.0x  |

The pre-refactor numbers are already flat in depth — each filter emits no
further notifications when blocked, so post-stage cost is bounded. But the
absolute floor was 8.8 µs because the *first* filter still ran on its own
intermediate cell. Post-refactor the predicate runs inside the fused closure
on the root cell, so a blocked emission costs only the root `ArcSwap::store`
plus closure dispatch — about 2.2 µs (close to the single-cell baseline).

## Observations

- **Per-stage propagation cost (was ~2.5 µs from ArcSwap+notify-per-cell):
  now near-zero**. Each pipeline stage adds only the closure call cost. Slope
  in `pure_chain_set` is ~30 ns per stage (closure dispatch + arithmetic),
  vs ~2.5 µs before.
- **Construction cost dropped substantially at all depths** (12x at depth 10,
  50x at depth 50, 30x at depth 500). The earlier 8 % gain at depth 500 was
  an O(N²) artefact of `&self`-receiver `MapExt::map` cloning the source
  pipeline at each call site. Consuming `self` makes per-stage allocation
  truly O(1), so depth-500 chain construction now drops from ~2.66 ms to
  ~92 µs.
- **Filter blocking is now near-baseline**: a blocked emission costs ~2.2 µs
  regardless of nominal chain depth (vs the ~8.8 µs floor pre-refactor).

## Methodology

- **Hardware**: AMD Ryzen 9 7900X3D 12-core (24 threads), Linux 6.19.10
  (Arch).
- **Toolchain**: rustc stable, `cargo bench` (criterion 0.5).
- **Sample count**: criterion default (100 samples per bench, 3 s warm-up,
  ~5 s collection).
- **Construction**: chains were built at compile time via `seq_macro::seq!`
  to preserve the distinct closure type at each `.map` (post-refactor each
  `.map` returns `MapPipeline<S, T, U, F>` so the runtime loop pattern
  `last = last.map(...)` no longer typechecks). Bench labels were preserved
  unchanged so criterion compares against the saved
  `pre-pipeline-refactor` baseline directly.
- **`#![recursion_limit = "2048"]`** added to both bench crates so the
  trait solver can verify the deep `MapPipeline<MapPipeline<...>>` types
  used at depth 500.

## Source

- Bench file: `hyphae/benches/pipeline_chains.rs`
- Latency bench (also adapted, no baseline): `hyphae/benches/latency.rs`
- Raw output: `/tmp/pipeline-comparison.log`, `/tmp/latency.log`
- Saved baseline: `target/criterion/<label>/pre-pipeline-refactor/`
