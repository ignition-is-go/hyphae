# Benchmarks

Benchmark source belongs in `hyphae/benches/` and reusable measurement tooling belongs in `tools/`. Generated measurements are local artifacts and are not committed.

## Run the maintained benchmarks

```sh
cargo bench -p hyphae --bench compiled_map_queries
cargo bench -p hyphae --features scheduler,region-calibration \
  --bench region_threshold_calibration
tools/bench-map-query-allocations.sh
tools/bench-operator-allocations.sh
```

Use Criterion filters and sampling flags after `--` when a focused run is sufficient. For example:

```sh
cargo bench -p hyphae --bench compiled_map_queries -- \
  'compiled_query/two_join_region/single' \
  --warm-up-time 1 --measurement-time 3 --sample-size 50 --noplot
```

Record the revision, command, toolchain, environment, confidence interval, and decision threshold in any durable report. Commit a compact report or derived table only when it explains a design decision that the source does not make obvious.

## Historical decisions

The files in [`historical/`](historical/) preserve the conclusions and small derived datasets from the 2026 static `MapQuery` work. Their raw logs, Criterion trees, profiles, and generated code captures are available from Git commit `4465c6dbe364bfee9f97136d64420549af14d800`; they are not required by builds or tests.

- [`static-map-query-evidence.md`](historical/static-map-query-evidence.md): allocation, code generation, latency, and compile-resource verdict.
- [`enabled-threshold.md`](historical/enabled-threshold.md): final parallel-entry threshold decision.
- [`join-region-threshold.md`](historical/join-region-threshold.md): earlier threshold calibration and its superseded conclusion.
- [`two-left-join-latency.md`](historical/two-left-join-latency.md): specialized two-stage latency evidence.
- [`region-sharding-phase-5a.md`](historical/region-sharding-phase-5a.md), [`region-sharding-phase-5b.md`](historical/region-sharding-phase-5b.md), and [`join-region-fail-stop.md`](historical/join-region-fail-stop.md): sharding and failure-semantics checkpoints.

Derived CSV and JSON inputs retained for audit are in [`historical/data/`](historical/data/).
