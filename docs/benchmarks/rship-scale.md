# The rship-scale suite

This suite is level 1 of the validation ladder: hyphae alone, through its
public API, inside one graph shaped like the rship server at load-test scale.
Each scenario reproduces a load that broke an earlier scheduler design. Use it
before a propagation or scheduling change goes to myko or rship, and after one
lands, to catch a regression.

It has two forms:

- **Tests.** `hyphae/tests/rship_scale.rs` runs every scenario at 1/40 of full
  size and asserts the correctness counts only. It is an ordinary test target
  and runs with `cargo test --all-features`. It takes about 20 seconds in a
  release build.
- **Timed runs.** The bench target `rship_scale` runs one scenario at full
  scale and prints one JSON line. It compiles only with the `scale-bench`
  feature, so no other command builds or runs it.

The code both forms share is `hyphae/benches/support/rship_scale/`.

## The graph at scale 1

About 246,000 materialized cells and 23,700 subscribers.

| part | size | role |
| --- | --- | --- |
| tick | 34 timer sources, 3,600 streams of 7 cells, 3,600 sinks | One write of 34 rows fans out to about 29,000 fires. Each stream is speed, multiply (a join with a parameter cell), add, offset, switch (a keyed lookup joined in), comp element, sink. |
| store | one `CellMap` of 45,000 rows, 3 cells per row, a subscriber on every eighth row | The hot source. Its cone is more than half the graph. |
| deep | 40 chains of 200 levels, tapped at level 100 | Deep, narrow propagation. |
| reconvergent | 900 regions of 22 cells, 4 sinks each | Diamonds and joins across unequal heights, half wired at random. |
| islands | 480 private sub-graphs of 64 cells and 8 sinks | Independent writers. |
| fan | one trigger, 5,000 chains of 5, 5,000 sinks | One trigger, thousands of streams. |
| feedback | 2,000 cells, a subscriber on each writes the next | Callback writes. |
| probe | one source, 5 cells, one sink | The independent small write. |

`--scale N` multiplies every part. `--bg F` multiplies only the parts the tick
does not touch. `--shrink F` divides every part.

## Scenarios

| name | what it does | validation ladder |
| --- | --- | --- |
| `tick` | The 30 Hz tick alone. Also measures setup time and memory. | S1 |
| `probe` | An independent small write, idle and then under the tick. | S2 |
| `shared` | A small write to a cell inside the tick's own scene. | S3 |
| `churn` | 1,000 operations a second beside the tick: half change a value, a quarter change a lookup key, a quarter add a keyed pipeline with a subscriber and remove the oldest. | S4 |
| `hotcone` | One row of the store rewritten 500 times a second. | |
| `feedback` | A trigger carried through thousands of callback writes. | |
| `setup` | The graph built from empty while the tick and the probe run. | |
| `writers` | `4 * scale` writers on private sub-graphs, paced and then closed-loop. | |
| `overload` | Eight writers saturate the engine for a third of the run. | |
| `diamond` | One writer, then four at once, into reconvergent graphs. | |
| `deep` | One 200-level chain per write, then 40 chains per write. | |
| `fanout` | One trigger to thousands of streams at 30 Hz. | |
| `oracle` | Four writers into reconvergent graphs and four into keyed inputs, then a replay that must observe the same sequence twice. | S7, reduced |
| `demand` | Keyed readers created and dropped in cycles. | S9 |
| `edit` | Streams added to and removed from the tick's scene. | S11 |
| `burst` | A control writer inserting 10,000 rows the tick does not read, then 1,000 it does. | S12 |
| `pulse` | A 1 kHz pulse train during the tick. | S13 |
| `cost` | The cost model. One writer, no tick: what each primitive costs to build, hold and fire, how a write's cost grows with depth, fan-out and the readers of a map, which writes of an equal value still propagate, and what contending writers lose. Not scaled. | |
| `diamonds` | Joins over parents that repeat their value, at widths 2, 8 and 64, nested, and with concurrent writers: evaluations per write, mixed inputs, stalls. Prints counts; `scripts/rship_scale.py` does not track it. | |
| `collection_reads` | A key cell, a membership cell and `get_value` read what a collection built in the same batch or callback stores. Test only. | |
| `tick --bg 10` | Ten times the state the tick does not touch. | S5 |
| `tick --workers N` | The tick on N threads. | S6 |

Every scenario checks correctness while it measures. A metric named `viol.*`
is a count that must be 0: a sink that missed a write, saw a writer's writes
out of order, saw half of a write, saw a glitch, or ended on the wrong value.
The test `checkers_detect_broken_rules` breaks each rule on purpose and fails
if a check stays silent.

The tick, the fan, the store and the reconvergent graphs are latest-value
cells. The probe, the pulse train, the islands, the deep chains and the
feedback chain carry events and are built inside `adapter::events`, which on
this version is `scheduler::no_coalesce`.

## Run the tests

```sh
cargo test -p hyphae --features scheduler --test rship_scale
```

## Run the timed suite

```sh
# One run of one scenario. The last line of output is the result.
cargo bench -p hyphae --features scale-bench --bench rship_scale -- --scen probe --scale 1

# Five runs of every scenario at scale 1, appended to
# benchmark-results/rship-scale/<commit>.jsonl.
scripts/rship_scale.py run

# Other scales, a subset, or a lock shared with other benchmarks on the machine.
scripts/rship_scale.py run --scales 1 2 4 --scenarios tick probe --lock /path/to/lock
```

Each result line carries the suite name, the commit (`-dirty` when the tree has
changes), the adapter name, the scenario, the scale, the worker count, the
number of cores other processes were using when the run started, and every
metric. Latencies are microseconds. A run that dies or exceeds the time limit
is recorded as a line with a `failed` field. `benchmark-results/` is ignored by
Git.

Run on a machine where other load is below one core if you can. When you
cannot, read `other_cores` in the result lines before you trust a difference.

## Compare with the baseline

```sh
scripts/rship_scale.py compare benchmark-results/rship-scale/<commit>.jsonl
```

The baseline is `docs/benchmarks/rship-scale-baseline.json`: for each scenario
and scale, the median, minimum and maximum of each metric over the baseline
runs, and the commit they came from.

`compare` exits 1 when it finds a regression. A regression is any of these:

- A correctness count that is 0 in the baseline is above 0 in any run.
- More runs died than in the baseline, or fewer than five finished.
- A tracked metric is worse than the baseline by more than its tolerance. The
  comparison is between medians. The metric must also be worse in every
  candidate run than in every baseline run, so that two overlapping ranges
  never count as a difference.

The tolerance of a metric is the larger of a floor and the baseline's own
range divided by its median. The floors are 25% for a p50, a time, a ratio or
a throughput, and 50% for a p99.

Why these numbers: the baseline was taken on a shared development machine
with 1 to 12 cores busy in other processes. Over five runs there, the range of
a metric divided by its median had the distribution in the table below. A
fixed floor below the usual range would flag noise, so the floor sits near the
median range and the baseline's own range covers the noisy metrics. On a quiet
machine, take a new baseline and the per-metric ranges tighten by themselves.

| kind of metric | metrics | median range / median | 75th percentile | 90th percentile |
| --- | --- | --- | --- | --- |
| p50 | 12 | 0.26 | 0.49 | 0.59 |
| p99 | 27 | 0.59 | 1.25 | 1.42 |
| setup time and memory | 3 | 0.04 | 0.08 | 0.08 |
| ratio | 5 | 0.16 | 0.24 | 0.26 |
| throughput | 4 | 0.28 | 1.34 | 1.34 |

The tracked metrics and the kind of each are the `TRACKED` table at the top of
`scripts/rship_scale.py`.

## Correctness counts that are not 0 in the baseline

At full scale v3.1.12 fails three correctness counts, and the baseline records
them. `compare` flags only a count that was 0 in the baseline.

| scenario | count | cause |
| --- | --- | --- |
| `churn`, `overload` | `viol.tick.gaps`, `viol.tick.incomplete` | The tick sources are latest-value cells. When a backlog forms, the scheduler replaces a queued write to a cell with the next one (`scheduler.rs`, `enqueue_locked` with `coalesce`), so some sinks never see some ticks. |
| `diamond` | `viol.probe.lost` | A probe write had not reached its sink 2 seconds after it was made, while four writers ran. The suite does not distinguish late from lost here. |

The reduced-scale tests pass because no backlog forms at 1/40 size.

## The absolute targets

```sh
scripts/rship_scale.py targets benchmark-results/rship-scale/<commit>.jsonl
```

`targets` reports each metric against the level-1 gate it has, not against the
baseline. The gates come from two numbers: a tick of 25,000 fires completes in
3.3 ms at p99 on 4 threads, and an independent small write completes in
0.25 ms at p99 while the tick runs. A target marked as growing with scale is
multiplied by the scale. A target passes only if every run passes.
v3.1.12 does not meet most of them. That is the reason the suite exists, and
`compare` is the command that guards against getting worse.

## Take a new baseline

Take a new baseline when a change moves a tracked metric on purpose, when the
suite itself changes what it measures, or when the benchmark machine changes.

```sh
git status                      # the tree must be clean, so the commit names the code
scripts/rship_scale.py run --runs 5 --scales 1
scripts/rship_scale.py compare benchmark-results/rship-scale/<commit>.jsonl   # read what moved
scripts/rship_scale.py baseline benchmark-results/rship-scale/<commit>.jsonl
git add docs/benchmarks/rship-scale-baseline.json
```

Say in the commit message which metrics moved and why. Do not take a new
baseline to make a regression disappear.

## Carry the suite to a branch that changes the write API

Scenarios never call a write function directly. They call
`benches/support/rship_scale/adapter_*.rs`: `write`, `post`, `flush`,
`from_callback`, `events`, `counters`, and the keyed-lookup type `Table`.
`mod.rs` names the adapter in one `#[path]` line. The directory holds three:

| file | for |
| --- | --- |
| `adapter_v3.rs` | this branch: `batch`, `switch_map`, `no_coalesce` |
| `adapter_settle_core.rs` | `hyphae-settle-core`: `submit` with an `Exclusive` key per writer, `flush`, `switch_map` |
| `adapter_h4.rs` | `h4`: `submit`, `flush`, `new_ticket` for callback writes, `family` and `lookup` |

Only the named adapter is compiled. A branch that changes the API changes the
`#[path]` line, and its baseline is taken on that branch.
