# lv-515f strict enabled-threshold evidence

## Decision

Final `lv-db06` audit rejected the prior 160,000 enter threshold because the
original calibration used a relaxed 1.05 decision rule and did not clear the
design's normative **1.5x confidence-bound speedup at the enabled threshold**.
The old result remains archived rather than rewritten. Production now enters
parallel mode at **200,000** estimated work and exits below **96,000**.

For the frozen four-stage workload each left row costs 97 units. Therefore
2,061 rows (199,917) are the last inactive serial case and 2,062 rows (200,014)
are the first enabled parallel case. The truth-table test proves those exact
branches and the unchanged exit bracket (989/990 rows, 95,933/96,030 units).

## Frozen command and environment

Candidate source is the tracked diff over `a63c70ae41c298d16ebde60848e96f0906e480bb`.
Every measurement was a fresh process on the same Ryzen 9 7900X3D host, Linux
7.1.3, rustc 1.97.1, the scheduler plus calibration features, Criterion 50
samples, 2 s warmup, and 5 s requested measurement:

```console
HYPHAE_WORKER_THREADS=<1|2|4> cargo bench -p hyphae \
  --features scheduler,region-calibration \
  --bench region_threshold_calibration -- \
  --warm-up-time 2 --measurement-time 5 --sample-size 50 --noplot \
  'inactive/2062$' --save-baseline lv515f-final-w<W>-r<R>
```

Each W/R pair has its own stdout, stderr, `benchmark.json`, `sample.json`,
`estimates.json`, and `tukey.json`. Criterion used flat sampling where a
50-sample linear schedule could not fit the target time; the displayed mean
interval is used in those runs, otherwise the displayed slope interval is
used. `estimates.csv` records the selected estimator exactly.

## Exact first-enabled-row result

95% confidence intervals, milliseconds per synchronous 2,062-row update:

| repeat | W1 sequential | W2 parallel | W4 parallel | W2 point / conservative floor | W4 point / conservative floor |
|---:|---:|---:|---:|---:|---:|
| 1 | 5.0645–6.0161 (5.5146) | 2.3803–2.5891 (2.4833) | 1.7965–1.9499 (1.8741) | 2.221x / **1.956x** | 2.943x / **2.597x** |
| 2 | 4.3061–5.1815 (4.7187) | 2.3961–2.5939 (2.4951) | 1.8451–2.2643 (2.0405) | 1.891x / **1.660x** | 2.312x / **1.902x** |

The conservative floor is `W1 lower95 / Wn upper95`. All four useful-core
comparisons exceed 1.5x at the exact first enabled row. The lowest observed
floor is **1.660x**, leaving 10.7% relative headroom over the gate.

## Correctness and interpretation

```console
cargo test -p hyphae --features scheduler,region-calibration \
  --test region_threshold_truth_table
cargo test -p hyphae --test four_join_application_preflight --all-features
```

Both pass. The application preflight retains final-state digest
`b394c0e6dc59b647f2dc1b05314b0043`, ordered-trace digest
`959cdbd3c843b5ae31f7575208113219`, exact output cardinality/order, and
synchronous settlement. The exit threshold remains unchanged. Raising only
the enter threshold makes formerly marginal 160k–200k inactive workloads stay
sequential; it does not alter active hysteresis, worker computation, caller
merge/publication, or large-batch execution.

**Go:** retain 200,000/96,000. This replaces the old threshold decision for
production and satisfies the literal Phase-5 enabled-threshold gate.
