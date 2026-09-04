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

## Final result

Commit `8ec08b9a6d78976596f9bc3767980f990162f722` produced:

```text
compiled_query/two_join_region/single
time: [2.6027 µs 2.6099 µs 2.6171 µs]
```

The final confidence interval overlaps the baseline, and the point estimate is 0.4% faster. Criterion detected no performance change. The latency gate passes.

The allocation harness also compared 100 single updates over 1,000 rows. Both revisions performed 702 allocations totaling 28,006 bytes and 1,700 deallocations totaling 99,894 bytes. Output cardinality and checksum matched. The recurring update path has no allocation regression.
