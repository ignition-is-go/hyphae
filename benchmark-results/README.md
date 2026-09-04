# Benchmark output

This directory is the default destination for local benchmark captures. Generated output is ignored by Git. Keep durable conclusions and the smallest data needed to review them in [`docs/benchmarks/`](../docs/benchmarks/README.md).

The allocation scripts create timestamped subdirectories here by default:

```sh
tools/bench-map-query-allocations.sh
tools/bench-operator-allocations.sh
```

Set `OUTPUT_DIR` to store a capture elsewhere. Criterion writes its generated measurements and reports under `target/criterion/`.

The raw captures removed from the working tree remain recoverable from Git commit `4465c6dbe364bfee9f97136d64420549af14d800`.
