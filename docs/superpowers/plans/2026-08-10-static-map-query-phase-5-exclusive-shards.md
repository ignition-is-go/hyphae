# Static MapQuery Phase 5 Exclusive Shards

**Date:** 2026-08-10

**Baseline:** deterministic groundwork (`8bc55ec`)

## Change

The coordinated two-join runtime starts in its original monolithic layout and
promotes to one complete join state per typed relation shard when a batch
crosses the measured threshold. Root diffs then route by their current join
key, each shard mutates its indexes and computes projections through exclusive
`&mut` state, and the first join's owned changes repartition directly into the
second join without an intermediate `MapDiff` clone. Rayon operates over
disjoint shard slices on Hyphae's shared dedicated executor.

Every input carries a stable ordinal. Shard output is merged by input ordinal,
intra-event diff phase, and local emission ordinal before one synchronous
publication. The diff phase is significant for a row moving between shards:
its old-shard removal must reach the next join before its new-shard insertion.

Promotion and parallel shard execution begin at 8,192 routed changes. Smaller
updates retain the already-optimized monolithic representation, avoiding both
Rayon dispatch and routing/merge overhead. Promotion is one-way and seeds the
shards from the monolithic typed indexes before applying the triggering diff.
Builds without the scheduler feature and wasm never promote.

## Validation

```console
cargo test -p hyphae --lib --features scheduler traits::collections::left_join
CARGO_BUILD_JOBS=1 cargo clippy -p hyphae --all-targets --all-features -- -D warnings
HYPHAE_WORKER_THREADS=0 cargo bench -p hyphae --features scheduler \
  --bench compiled_map_queries -- --save-baseline phase5-sharded-seq2 \
  'compiled_query/two_join_region/batch/(1000|10000)$'
HYPHAE_WORKER_THREADS=4 cargo bench -p hyphae --features scheduler \
  --bench compiled_map_queries -- --baseline phase5-sharded-seq2 \
  'compiled_query/two_join_region/batch/(1000|10000)$'
```

The focused suite includes a 512-step deterministic differential test over
mixed insert, update, and removal operations on all three roots. A targeted
test moves a left row across the first join's shards and then moves its
intermediate row across the second join's shards.

## Results

| Workload | Pre-shard checkpoint | Adaptive exclusive shards | Change |
| --- | ---: | ---: | ---: |
| Two joins, one update | 3.3590-3.3738 us | 3.3164-3.3330 us | **-1.45%** |
| Two joins, batch 10 | 7.2256-7.2700 us | 7.1157-7.1549 us | **-1.43%** |
| Two joins, batch 100 | 48.615-48.913 us | 46.725-47.047 us | **-3.37%** |
| Two joins, batch 1,000 | 441.28-444.71 us | 436.27-439.46 us | -0.62%; within noise |
| Two joins, batch 10,000 | approximately 4.67 ms | 3.9574-4.0924 ms | **-17.6%** |

Before adaptive promotion, permanent sharding regressed the single-through-1k
curve by 7.6-19.9%. Keeping the monolithic layout until the crossover eliminates
that cost. Consuming the first join's owned intermediate vector also reduced
the sharded forced-sequential 10,000-row midpoint from approximately 5.81 ms
to 5.54 ms. Four workers are the measured optimum on this host; eight workers
measured slower because the plan has only enough coarse work for four useful
partitions.

The final 10,000-row measurement includes one-time promotion, index seeding,
routing, parallel mutation, and stable merge. Repeated large batches retain the
promoted representation and avoid the seeding cost.

## Decision

Keep adaptive exclusive typed shards and the 8,192-change crossover. The final
candidate improves every measured point on the curve and makes the 10,000-row
end-to-end workload 17.6% faster. Isolated sharded execution is 25.2% faster
with four workers than with workers disabled (1.34x), still below the design's
aspirational 1.5x scaling gate. Further gains require reducing route and merge
overhead or carrying partition identity through longer compiled regions.
