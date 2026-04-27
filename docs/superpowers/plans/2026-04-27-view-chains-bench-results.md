# View-Chain Benchmark Results (Realistic Application Shapes)

Date: 2026-04-27

## Summary

`view_chains.rs` exercises the chain shapes seen in real reactive
applications — multi-hop joins + projections + group_by over event-driven
entity tables, with realistic data shapes (`Arc<str>` keys, `Arc<Entity>`
values). The results are an honest end-to-end check that all the
refactor wins (Pipeline / MapQuery / consume-self / lock-free subs /
materialize-no-op / share) translate to representative load.

**Headline:** at 1k-entity scale the dominant view shape (4-hop with two
joins and a project) updates in **3.7 µs**. At a frame-rate budget that
allows roughly **4,500 entity updates per 60Hz frame** through a single
view. Wide fan-out (100 subscribers) adds ~15% on top, validating the
lock-free subscriber dispatch under realistic fan-out.

## Scenarios

Six scenarios chosen from the patterns observed in production reactive
code (libs/entities/* in rship):

| scenario | shape | source maps |
|---|---|---|
| `mid_view_4hop` | `instances.left_join_by(lanes).left_join_by(keyframes).project()` | 3 |
| `assets_view_5hop` | `files.group_by().left_join_by(transfers).project().left_join_by(metadata).project()` | 3 |
| `deep_view_7hop` | 3 × `left_join_by` + 3 × `project` (SessionTargets pattern) | 4 |
| `fan_out_mid_view` | mid_view + N diff subscribers | 3 + N subs |
| `batch_mutation_mid_view` | `insert_many` of K rows through mid_view | 3 |
| `select_project` | `targets.select(predicate).project(reshape)` | 1 |

Entities use `Arc<str>` keys and `Arc<EntityStruct>` values; structs
include a monotonic `seq: u64` field on each iter to ensure every
mutation produces structurally distinct data (defeats the
`apply_diff_owned` no-op short-circuit, which would otherwise hide real
costs once a benchmark's modular cycle aligned).

## Headline Numbers

Mean estimates from criterion (sample_size 50; 30 for `deep_view_7hop`).

### `mid_view_4hop` — 3-source-map view (per source insert)

| source rows | per-insert |
|---:|---:|
| 100 | 3.38 µs |
| 1,000 | 3.72 µs |
| 10,000 | 7.15 µs |

### `assets_view_5hop` — group_by + 2 joins + 2 projects

| source rows | per-insert |
|---:|---:|
| 100 | 5.31 µs |
| 1,000 | 5.74 µs |
| 10,000 | 8.87 µs |

### `deep_view_7hop` — 3 joins + 3 projects, mutating action through deepest stage

| source rows | per-insert |
|---:|---:|
| 100 | 5.45 µs |
| 1,000 | 6.80 µs |
| 5,000 | 11.83 µs |

### `fan_out_mid_view` — mid_view + N diff subscribers

| subs | per-insert | overhead vs 1 sub |
|---:|---:|---:|
| 1 | 3.72 µs | — |
| 5 | 3.68 µs | -1% |
| 20 | 3.75 µs | +1% |
| 100 | 4.28 µs | +15% |

Flat from 1 → 20 subscribers (the ArcSwap-loaded subscriber Vec dispatch
is essentially free at this scale). Modest growth at 100 subs is the
actual N callback invocations.

### `batch_mutation_mid_view` — `insert_many` through mid_view (1k entities)

| batch | total | per-row |
|---:|---:|---:|
| 1 | 3.81 µs | 3.81 µs |
| 10 | 23.83 µs | 2.38 µs |
| 100 | 229 µs | 2.29 µs |

Per-row drops ~38% once batched ≥10. `apply_batch` amortizes the diff
coalescing.

### `select_project`

| target rows | per-insert |
|---:|---:|
| 100 | 2.39 µs |
| 1,000 | 2.45 µs |
| 10,000 | 2.91 µs |

## Frame-Budget Translation

At 60 Hz (16.6 ms/frame), the throughput each view supports for
single-source-map mutations:

| scenario | scale | updates/frame |
|---|---|---:|
| `select_project` | 10k targets | ~5,700 |
| `mid_view_4hop` | 1k entities | ~4,500 |
| `mid_view_4hop` | 10k entities | ~2,300 |
| `assets_view_5hop` | 10k files | ~1,900 |
| `deep_view_7hop` | 5k targets | ~1,400 |
| `mid_view_4hop` + 100 subs | 1k entities | ~3,900 |

These are per-view budgets. Real applications run many such views
concurrently — but each one is independent (no shared state, no
synchronization), so the budgets compose linearly until the propagation
work saturates a core.

## Methodology

- Hardware: AMD Ryzen 9 7900X3D
- Criterion default config; `sample_size(50)` for most scenarios,
  `sample_size(30)` for `deep_view_7hop`
- Baseline saved as `post-everything` for future comparison via
  `cargo bench --bench view_chains -- --baseline post-everything`
- Bench source: `hyphae/benches/view_chains.rs` (commit `fc032ad`)

## Related Bench Reports

- `2026-04-23-pipeline-type-bench-results.md` — single-cell pipeline
  results vs `pre-pipeline-refactor` baseline (40-200× wins)
- `2026-04-25-cell-map-query-plans-bench-results.md` — CellMap join-chain
  results vs `pre-cellmap-query-plans` baseline (4-7× wins)

This view_chains run is the first to measure realistic application-shaped
load end-to-end after all refactors land. Per-update costs land in the
2-12 µs range depending on view depth and scale, which translates to
1,400-5,700 single-view updates per 60Hz frame — well above what real
workloads need.
