# Benchmarks

Benchmark source belongs in `hyphae/benches/` and reusable measurement tooling belongs in `tools/`. Generated measurements are local artifacts and are not committed.

## Run the maintained benchmarks

```sh
cargo bench -p hyphae --bench compiled_map_queries
cargo bench -p hyphae --features region-calibration \
  --bench region_threshold_calibration
tools/emission-bench.py run
```

`tools/emission-bench.py` reports nanoseconds and allocations per emission; see [`emission/`](emission/README.md) for its baseline.

Use Criterion filters and sampling flags after `--` when a focused run is sufficient. For example:

```sh
cargo bench -p hyphae --bench compiled_map_queries -- \
  'compiled_query/two_join_region/single' \
  --warm-up-time 1 --measurement-time 3 --sample-size 50 --noplot
```

Record the revision, command, toolchain, environment, confidence interval, and decision threshold in any durable report. Commit a compact report or derived table only when it explains a design decision that the source does not make obvious.

Earlier benchmark reports and their raw data are in Git history.
