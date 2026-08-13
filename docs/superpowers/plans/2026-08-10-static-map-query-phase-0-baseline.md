# Static MapQuery Phase 0 Baseline

**Date:** 2026-08-10
**Runtime commit:** `7fcb1421a36641e905cdcb0c30efa7867d03d6c0`
**Purpose:** Immutable “before” evidence for
`2026-08-10-static-compiled-map-query-design.md`
**Criterion baseline:** `static-mapquery-before-7fcb142` and
`compiled-query-before-7fcb142`

## Environment

```text
CPU: AMD Ryzen 9 7900X3D 12-Core Processor
Kernel: Linux 7.1.3-arch2-2 x86_64 GNU/Linux
rustc: 1.97.1 (8bab26f4f 2026-07-14)
cargo: 1.97.1 (c980f4866 2026-06-30)
Profile: release/bench, locked dependencies
Allocation build jobs: 1
```

The machine was otherwise idle. Criterion used its default warmup and 100
samples except `view_chains`, whose source config uses 50 samples for most
cases and 30 for the deep view.

## Commands

```console
cargo bench -p hyphae --bench cell_map_chains -- \
  --save-baseline static-mapquery-before-7fcb142

cargo bench -p hyphae --bench view_chains -- \
  --save-baseline static-mapquery-before-7fcb142

cargo bench -p hyphae --bench compiled_map_queries -- \
  --save-baseline compiled-query-before-7fcb142

REVISION=HEAD tools/bench-map-query-allocations.sh

CARGO_BUILD_JOBS=1 \
CARGO_TARGET_DIR=/tmp/hyphae-static-mapquery-baseline-target \
time cargo build --locked --offline --release -p hyphae \
  --bench compiled_map_queries
```

The dedicated harness was added without modifying the runtime. Its SHA-256 is
`e9dc04a1c38915366666b48bcb165faa9ecd1ecb94f777a14adb1688d7eb05f1`.
The allocation harness SHA-256 is
`a2122c39426e0aa6cf4e46af93f8c92a791cca1b691e99c8dfb6fb70731d139f`.

## Dedicated compiled-query latency

All values are Criterion 95% confidence intervals.

| Scenario | Before |
| --- | ---: |
| Four-stage key-preserving projection region, one update | 3.5343–3.5533 µs |
| Two joins separated by projection, one update | 4.1174–4.1352 µs |
| Four joins reusing one logical right relation, one update | 5.3422–5.3721 µs |
| Rekey between two joins, one update | 4.1357–4.1581 µs |
| Two-join batch, 1 row | 4.1573–4.1744 µs |
| Two-join batch, 10 rows | 15.731–15.813 µs |
| Two-join batch, 100 rows | 133.38–134.37 µs |
| Two-join batch, 1,000 rows | 1.2220–1.2350 ms |

Every benchmark reads the affected output key after mutation, preventing the
output graph and update work from being optimized away.

## Existing join-chain latency

| Join depth | Before |
| ---: | ---: |
| 1 | 3.2614–3.2789 µs |
| 2 | 3.5315–3.5448 µs |
| 3 | 3.8413–3.8665 µs |
| 5 | 4.4504–4.4721 µs |

## Existing application-shaped view latency

| Scenario | Scale | Before |
| --- | ---: | ---: |
| `mid_view_4hop` | 100 | 4.2006–4.2277 µs |
| `mid_view_4hop` | 1,000 | 4.4708–4.5130 µs |
| `mid_view_4hop` | 10,000 | 6.1746–6.4537 µs |
| `assets_view_5hop` | 100 | 7.0311–7.0833 µs |
| `assets_view_5hop` | 1,000 | 7.3464–7.4262 µs |
| `assets_view_5hop` | 10,000 | 8.4427–8.7061 µs |
| `deep_view_7hop` | 100 | 5.9342–5.9907 µs |
| `deep_view_7hop` | 1,000 | 6.5968–6.7814 µs |
| `deep_view_7hop` | 5,000 | 10.407–10.699 µs |
| `fan_out_mid_view` | 1 subscriber | 4.5266–4.6003 µs |
| `fan_out_mid_view` | 5 subscribers | 4.5271–4.5884 µs |
| `fan_out_mid_view` | 20 subscribers | 4.5973–4.6422 µs |
| `fan_out_mid_view` | 100 subscribers | 4.9111–4.9811 µs |
| `batch_mutation_mid_view` | 1 row | 4.5250–4.5841 µs |
| `batch_mutation_mid_view` | 10 rows | 17.410–17.614 µs |
| `batch_mutation_mid_view` | 100 rows | 143.58–146.02 µs |
| `select_project` | 100 | 3.5991–3.6203 µs |
| `select_project` | 1,000 | 3.6211–3.6485 µs |
| `select_project` | 10,000 | 3.2841–3.3778 µs |

## Allocation and retention baseline

The full raw output and environment live under
`benchmark-results/map-query-allocations/20260810T095124Z/`.

### Four-stage projection region

| Phase | Alloc calls | Alloc bytes | Dealloc calls | Dealloc bytes | Net bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| Build | 4 | 64 | 0 | 0 | 64 |
| Materialize | 7,557 | 1,408,688 | 4,388 | 823,220 | 585,468 |
| 100 single updates | 3,000 | 168,800 | 2,998 | 184,688 | -15,888 |
| One 100-row batch | 865 | 73,336 | 716 | 61,084 | 12,252 |
| Teardown | 0 | 0 | 3,223 | 574,152 | -574,152 |

The output contains 500 rows, matching the predicate's 50% selectivity.

### Two-join region

| Phase | Alloc calls | Alloc bytes | Dealloc calls | Dealloc bytes | Net bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| Build | 2 | 32 | 0 | 0 | 32 |
| Materialize | 28,212 | 5,479,040 | 18,754 | 3,613,292 | 1,865,748 |
| 100 single updates | 2,800 | 236,800 | 3,798 | 308,688 | -71,888 |
| One 100-row batch | 1,556 | 193,040 | 1,357 | 177,188 | 15,852 |
| Teardown | 0 | 0 | 10,836 | 2,087,000 | -2,087,000 |

The output contains 1,000 rows.

### Four joins over one repeated logical relation

| Phase | Alloc calls | Alloc bytes | Dealloc calls | Dealloc bytes | Net bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| Build | 4 | 64 | 0 | 0 | 64 |
| Materialize | 72,028 | 12,450,600 | 54,257 | 8,907,328 | 3,543,272 |
| 100 single updates | 4,800 | 445,600 | 5,798 | 517,488 | -71,888 |
| One 100-row batch | 2,995 | 365,368 | 2,796 | 348,972 | 16,396 |
| Teardown | 0 | 0 | 18,013 | 3,622,260 | -3,622,260 |

The output contains 1,000 rows. This is the primary index-reuse baseline: the
current installer builds four independent right-side relationship indexes.

## Compile resources

The representative dedicated harness was built from a clean isolated target
directory with one build job.

| Metric | Before |
| --- | ---: |
| Wall time | 46.83 s |
| User CPU | 42.78 s |
| System CPU | 4.37 s |
| Peak RSS | 417,160 KiB |
| Benchmark executable | 4,338,864 bytes |

Compile-resource growth is accepted by the design but must remain recorded in
every after report.

## Phase 0 decision

The baseline is suitable for implementation. It covers the required
key-preserving region, multi-join region, repeated relationship, rekey,
sequential small update, and 1–1,000-row batch shapes. Runtime changes may now
begin. Each implementation phase must compare against the relevant rows above
and archive a fresh after report.
