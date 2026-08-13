# lv-33ed TwoLeftJoin latency evidence

Date: 2026-08-11 UTC  
Base: `3d4db4416f5e8cbd9cd06010faf029e0c4d3011c` (working-tree candidate; not committed)

## Result

Exact current-HEAD baseline, three fresh default Criterion processes:

- 3.4783–3.4959 us
- 3.4968–3.5246 us
- 3.4884–3.5203 us

Final atomic-left candidate, fresh default Criterion processes:

- 2.7235–2.7470 us
- 2.7284–2.7516 us
- 2.7434–2.7589 us

An independent audit found that the reusable impacted-key buffer retained the
last callback's key elements rather than only its capacity. After clearing the
elements on every return path and adding a regression assertion, a fresh
100-sample process measured **2.7438–2.7666 us** (`final-retention-fix.txt`).
This audited final upper bound is below the strict **2.88218 us** gate by
115.58 ns. Against the frozen v3 lower bound 4.1174 us, the conservative
improvement is **32.81%**.

Raw Criterion `benchmark.json`, `sample.json`, `estimates.json`, and reports for
the audited final run are under
`criterion-raw/compiled_query_two_join_region_single/retention-fix/`.

## Isolated decisions

- OrderedSet sibling-buffer reuse: 3.4743–3.4975 us, within noise; retained because
  it removes recurring allocation and composes with direct propagation.
- maintained join-key-grouped rows + borrowed slices: 3.3200–3.3437 us.
- fused stage-1 -> stage-2 sequential propagation: 3.2896–3.3131 us.
- owned CellMap batch application/publication: 2.8452–2.8722 us.
- specialized atomic left-root update: 2.7235–2.7470 us.

## Regression/correctness

- repeated typed four-join right-root tiny: 4.5119–4.5710 us; Criterion change
  +0.32%..+1.66%, within the strict <3% regression gate.
- two-join batch/1: 2.9290–2.9560 us, improved 17.2%..17.9%.
- `cargo test -p hyphae --lib traits::collections::left_join`: 22 passed.
- `cargo test --workspace --all-features`: all unit/integration/doc tests passed after audit fix.
- `cargo test -p hyphae --no-default-features`: passed after audit fix.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed after audit fix.
- `cargo fmt --all -- --check`: passed after audit fix.
- Reusable impacted-key regression asserts elements are released while capacity is retained.

## Exact benchmark commands

```sh
cargo bench -p hyphae --bench compiled_map_queries -- \
  'compiled_query/two_join_region/single'
HYPHAE_QUERY_WORKERS=1 cargo bench -p hyphae --bench compiled_map_queries -- \
  'compiled_query/(two_join_region/batch/1$|repeated_typed_relation_four_join/repeated_right_single)'
```

Default Criterion settings were used: 3 s warmup, 100 samples, approximately
5 s measurement. Every reported repeat was a fresh `cargo bench` process.

## Environment

```
base_commit=3d4db4416f5e8cbd9cd06010faf029e0c4d3011c
rustc 1.97.1 (8bab26f4f 2026-07-14)
cargo 1.97.1 (c980f4866 2026-06-30)
Linux 7.1.3-arch2-2 x86_64 GNU/Linux
Model name:                              AMD Ryzen 9 7900X3D 12-Core Processor
19	33	hyphae/src/cell_map.rs
196	46	hyphae/src/traits/collections/internal/join_runtime.rs
10	0	hyphae/src/traits/collections/internal/ordered_set.rs
```

## Source integrity

Working diff SHA-256: `7fe6477df6c1d777fad13ccd906a688b8bd3a99006383ca5944309508d83036b`

- `hyphae/src/cell_map.rs`: `37bea3ebe758d7b735da20fc16c05d5ba93b87b4ec78a2b9c13dfcb5373d4654`
- `hyphae/src/traits/collections/internal/join_runtime.rs`: `e0028d7ae501e87ba41657571faab781cb54540cde586a04f205f1608065c2f0`
- `hyphae/src/traits/collections/internal/ordered_set.rs`: `256a9e7d52978bff3d484da94c33ec86bca0042dd5e29d92c60211d8b87cfd78`


## Remaining evidence

Allocation, generated-code/profile, and serial-compile evidence remains explicitly
tracked by **lv-671e**. It was not fabricated here, and the pre-existing untracked
`20260810T165500Z-baseline` allocation artifact was not touched. lv-33ed should
remain open until the parent decides whether lv-671e satisfies that evidence
portion of its acceptance criteria.
