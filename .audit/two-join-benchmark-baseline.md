# Two-join benchmark baseline

Commit `4465c6dbe364bfee9f97136d64420549af14d800` produced this Criterion interval on the current machine:

```text
compiled_query/two_join_region/single
time: [2.6117 µs 2.6201 µs 2.6282 µs]
```

Command:

```sh
cargo bench -p hyphae --bench compiled_map_queries -- \
  'compiled_query/two_join_region/single' \
  --sample-size 50 \
  --warm-up-time 1 \
  --measurement-time 3 \
  --noplot
```

The remediation passes this gate when the final confidence interval overlaps this baseline or its point estimate is no more than 5% slower on the same machine. A faster result passes.
