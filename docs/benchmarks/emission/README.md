# Emission bookkeeping records

Results of the `emission` bench (`hyphae/benches/emission.rs`), captured with
`tools/emission-bench.py`.

| file | what it is |
| --- | --- |
| `baseline.json` | The historical "before": the harness on top of e977a47, before the emission fixes. It has no `fused_mixed` case, which was added later. |
| `current.json` | The current reference: the tree after the emission fixes (commit recorded in the file). Compare new work against this one. |

Both were captured with:

```sh
tools/emission-bench.py run --runs 5 --cpus 6-11 --out <file>
```

Compare two records with `tools/emission-bench.py compare BEFORE AFTER`.

allocs/op is the stable metric. It is exact for single-threaded cases, except
`fan_out/128`, which varies by a few allocations between runs of the same
binary. ns/op depends on machine load: `baseline.json` ran at a 1-minute load
average of about 13, `current.json` at about 24 (both recorded in the files
as `loadavg_before`). Treat time differences under about 8% as noise, and
recapture both sides on the same machine and CPU set before comparing times.
