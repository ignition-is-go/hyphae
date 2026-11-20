# What hyphae v3.1.12 costs at rship scale

This is for people who write rship and myko code on top of hyphae. It says
what each primitive costs to build, to hold and to fire, how a write's cost
grows with what it reaches, what is merged or lost when writers contend, and
which patterns to use and avoid. Every number was measured on v3.1.12 with
the `scheduler` feature, release build, 4 worker threads, one writer inside a
`batch` unless a row says otherwise. The builds used Rust 1.97.1, the
toolchain the repository pins, except the dedup comparison near the end,
where both sides were built with 1.100.0.

Numbers are medians over the runs, with the range and the run count in the
tables at the end. The machine was shared: 2 to 24 cores were busy in other
processes during the runs, so read a single number as good to about 25% and a
p99 as good to a factor of two. The `cost` rows are the best of three repeats
inside each run, which removes most of that noise.

## The short version

- **A cell is 1.2 KB and about 150 ns per write that reaches it.** A join is
  3 KB. The 246,000-cell graph is 333 MB.
- **A write costs about 110 ns plus 150 ns per cell it reaches**, as long as
  no cell on the way has a thousand direct dependents. Above that it is 400 to
  900 ns per cell. The 30 Hz tick, 34 sources fanning out to about 25,000
  cells, takes 12.5 ms: 500 ns per cell and 37% of the frame.
- **Only one thread propagates at a time.** A 5-cell write that takes 3 µs on
  an idle graph takes 13.5 ms at p99 while the tick runs. Four writers get 58%
  of the throughput of one.
- **Equal values propagate.** A `map` or `join` whose result did not change
  still fires everything below it. Where a stage repeats its value, that is
  most of the work: 61% of a tick in the measured case.
- **Readers of one key are free for everyone else. Subscribers to diffs are
  not.** A row write costs 140 ns with 100,000 `get(k)` readers of other keys
  on the map, and 85,000 ns with 10,000 `subscribe_diffs` subscribers.
- **Value cells merge writes under contention and event cells do not, but
  event cells glitch at joins.** Nothing loses its final value.

## Cost per primitive

| primitive | to build | to hold | to fire once |
| --- | --- | --- | --- |
| source cell (`Cell::new`) | 350 ns | 425 B | 117 ns to write in a batch, 31 ns unbatched |
| derived cell (`map` + `materialize`) | 1.3 µs | 1.2 KB | 150 ns in a batch, 60 to 90 ns unbatched |
| ten `map`s fused into one `materialize` | 15.5 µs | one cell | 332 ns for all ten |
| `join` + `map` + `materialize` | 4.0 µs | 3.0 KB | 400 ns |
| keyed lookup (`switch_map` over `map.get`) | 1.9 µs | not measured | 660 ns when its row changes, 1,230 ns when its key changes, 0 when another row changes |
| `map.get(k).materialize()` | 250 ns, the same with 1,000 or 100,000 other readers | not measured | fires only for its own key |
| `subscribe` on a cell | 350 ns | 224 B | 4 to 6 ns |
| `subscribe_diffs` on a map | 3.6 µs on 100 rows, 118 µs on 10,000 rows | not measured | 8.5 ns per row write, whichever row |
| row in a `CellMap` | 587 ns written alone, 41 ns in `insert_many` | not measured | 130 ns per write, with 1,000 rows or 100,000 |

Dropping costs 0.1 to 1.6 µs per item. Dropping a keyed lookup costs as much
as building one.

## How cost grows

**Depth.** Linear. A chain costs 150 ns per cell in a batch at 10, 100 and
1,000 cells. A 200-cell chain in the full graph takes 78 µs. Very long chains
have a limit of their own: dropping a 100,000-cell chain overflowed an 8 MB
stack, because the drop recurses once per cell.

**Fan-out to derived cells.** Not linear. One source with N derived cells
costs, per derived cell per write, in a batch:

| N | 10 | 100 | 1,000 | 10,000 | 100,000 |
| --- | --- | --- | --- | --- | --- |
| ns per derived cell | 137 | 202 | 750 | 868 | 1,701 |

The scheduler hands a wave to its worker threads from 256 cells at one height
(`DEFAULT_WAVE_THRESHOLD`), which lies between 100 and 1,000. One run with a
single worker thread measured 402 and 421 ns at 1,000 and 10,000, so part of
the step is the hand-off. What causes the rest was not measured. Unbatched,
the same write costs 45 to 90 ns per cell up to 10,000.

**Fan-out to subscribers.** Flat. 4 to 6 ns per subscriber from 100 to
100,000 subscribers on one cell.

**Readers of one map that do not match the write.** It depends on how they
read:

| readers of the map | cost of one row write |
| --- | --- |
| none | 130 ns |
| 100,000 `get(k)` readers of other keys | 139 ns, and none of them evaluates |
| a `len()` reader | 129 ns |
| a `map_values` view | 290 ns, with 1,000 rows or 10,000 |
| 1,000 `subscribe_diffs` subscribers | 9.0 µs |
| 10,000 `subscribe_diffs` subscribers | 85 µs |
| an `entries()` reader on 10,000 rows | 3.9 µs: the whole map is copied per write |

**Consumers that each test one shared cell.** This is the expensive form of
"non-matching subscribers". N cells each compute `source == mine`, with three
cells below each. Every write evaluates all N tests, and because an unchanged
`false` still propagates, all 3N cells below fire too:

| N | per write, as built | per write with `.deduped()` after the test |
| --- | --- | --- |
| 100 | 86 µs | 2.8 µs |
| 10,000 | 29 ms | 0.30 ms |
| 100,000 | 384 ms | 18 ms |

**Total state.** A row write does not slow down with the size of the map or
the number of keyed readers. The tick does slow down as the graph around it
grows: per cell it costs 500 ns at scale 1, 590 ns at scale 2 and 680 ns at
scale 4, with the tick itself growing in proportion.

## What a write costs, by what it reaches

Add these up:

| part of the write | cost |
| --- | --- |
| the `batch` itself | 7 ns empty, about 110 ns with one `set` |
| each derived cell reached, narrow graph | 150 ns |
| each derived cell reached below a source with 1,000 or more direct dependents | 400 to 900 ns |
| each `join` reached | 400 ns |
| each keyed lookup whose row changed | 660 ns |
| each keyed lookup whose key changed | 1,230 ns |
| each plain subscriber | 5 ns plus its own work |
| each `subscribe_diffs` subscriber on a written map | 8.5 ns |
| each `entries()` reader on a written map | 0.39 ns per row of the map |
| each cell below an unchanged result | the same as below a changed one |

Checked against the full graph: the tick reaches about 25,000 cells, a join
and a lookup per stream, and takes 12.5 ms, which is 500 ns per cell. Forty
chains of 200 in one write (8,000 cells) take 1.9 ms, 240 ns per cell. One
chain of 200 takes 78 µs, 390 ns per cell. A 64-cell write takes 34 µs,
530 ns per cell. Small writes carry more fixed cost per cell.

What a write waits for is separate from what it costs. One thread drains the
scheduler's queue at a time, and a write made while it drains joins that
queue:

| a small independent write | p50 | p99 |
| --- | --- | --- |
| idle graph | 2.9 µs | 35 µs |
| while the 30 Hz tick runs | 11 µs | 13.5 ms |
| during overload from eight other writers | not reported | 87 ms |

A store row write with 45,000 keyed readers goes from 13 µs at p99 idle to
5.2 ms under the tick. Four writers on private sub-graphs complete 56,000
writes a second together, against 95,000 for one writer alone.

## What is merged, what is lost, and when

Nothing in any run lost its final value. What changes is which intermediate
values a subscriber sees, and when a writer's own write becomes visible.

| situation | what happens | measured | verdict |
| --- | --- | --- | --- |
| several `set`s on one value cell inside one `batch` | The last wins. Subscribers see one value. | 10 sets, 1 callback | A latest-value merge. Intended. |
| the same on a `no_coalesce` cell | Every value is delivered. | 10 sets, 10 callbacks | Intended. |
| one thread, one `batch` per write, nobody else writing | Every write is delivered, in order. | 0 of 20,000 missed | Correct. |
| two or more threads, each writing its own value cell, one `batch` per write | A `batch` that closes while another thread drains returns at once and leaves its write queued. The writer's next write to the same cell replaces it. | 85% of writes never reach the sink with 2 threads, 96% with 4, 99% with 8. The last value always does. | A latest-value merge, but it is caused by another writer's activity on unrelated cells, and it starves any logic that counts or reacts to each value. Not acceptable for a cell that carries events. |
| the same, read back by the writer | The writer reads a derived cell right after its `batch` returns and gets the old value. | 88% of reads stale with 2 threads, 97% with 4, 99% with 8. On `no_coalesce` cells 50%, 75%, 88%. | A writer cannot read its own write. Not acceptable where code reads back. |
| the 30 Hz tick while eight other writers saturate the queue | The tick's sources are value cells. Queued ticks are replaced by later ones. | 140,400 of 594,000 sink deliveries missed (24%), tick p99 37.5 ms, the queue 67 ms to drain afterwards. | A latest-value merge of frames. The cause is the backlog. |
| the tick during 1,000 structural operations a second | The same, rarely. | 0 in the median run, 7,200 in the worst of 5 | The same. |
| a 1 kHz pulse train on `no_coalesce` cells during the tick | Every pulse is delivered, late. | 0 of about 5,500 missed, p99 11 ms | Nothing lost. The delay is the single drain. |
| writes on `no_coalesce` cells that meet at a join | Each write is its own operation, so the join fires once per arrival and sees one input updated and the other not. | 1,997 glitched observations in 1,998 writes, single thread | Event cells are not glitch-free. Do not join them in a diamond. |
| a write outside any `batch` | Eager, depth-first. A join fires once per parent that changed. | A 2-wide diamond fires twice per write, a 64-wide one 64 times, with mixed inputs each time. | Use a `batch`. |
| a subscriber callback that writes | Delivered in order. | 2,000-link feedback chain, 114 ns per link, 0 lost | Correct. |

## Equal values

| write | callbacks below, v3.1.12 |
| --- | --- |
| `Cell::set` of the value the cell holds | 1 |
| `map` whose result did not change | 1 |
| `join` whose result did not change | 1 |
| keyed lookup whose key moved to a row with the same value | 1 |
| `map(...).deduped()` whose result did not change | 0 |
| `CellMap::insert` of the value the row holds, to a `get(k)` reader | 0 |
| the same, to `subscribe_diffs` | 0 |
| `map_values` whose output row did not change, as a diff | 0 |

Collections already drop repeats. Cells and every operator that ends in a
cell do not.

## Patterns for rship

**Use these.**

- **`map.get(k)` for anything keyed.** 250 ns to build, and a write to
  another row costs it nothing: 139 ns per row write with 100,000 such
  readers.
- **`.deduped()` straight after a stage that quantises, thresholds, selects
  or compares.** It is fused into the same cell and costs nothing extra. In a
  3,600-stream tick where one stage repeats 7 ticks in 8, it took the tick
  from 10.9 ms to 4.2 ms.
- **Fuse `map`s before `materialize`.** Ten maps in one cell cost 332 ns per
  write and one cell of memory. Ten cells cost 1,500 ns and 12 KB.
  Materialize where something subscribes or branches, not at every step.
- **`insert_many` for loads.** 41 ns per row against 587 ns for single
  inserts.
- **Plain subscribers for wide fan-out of side effects.** 5 ns each.
- **One `batch` per logical write.** It costs 110 ns and makes every join
  fire once on consistent inputs.
- **`no_coalesce` for a cell whose every value must be seen**, and keep that
  subgraph free of joins that reconverge.

**Avoid these.**

- **N consumers each filtering one shared cell.** 29 ms per write at 10,000
  consumers. Key the data in a `CellMap` and give each consumer `get(k)`, or
  at least put `.deduped()` after the test (0.30 ms).
- **Many `subscribe_diffs` on one busy map, each filtering in its callback.**
  8.5 ns per subscriber per row write, and 12 ns per row of the map to
  subscribe. 1,000 subscribers on a map written 25,000 times a second is
  0.2 of a core. Use `get(k)`, or one subscriber that routes.
- **`entries()` or `items()` as a reactive value on a large, busy map.** The
  whole map is copied per write: 3.9 µs per write at 10,000 rows.
- **One cell with thousands of derived cells directly below it, on a hot
  path.** 750 to 1,700 ns per derived cell per write, five to ten times the
  narrow-graph cost. This is the shape of the tick.
- **A cell per step.** 1.2 KB and 150 ns each. The census quoted in
  `first-principles.md` counted 252,884 cells from 127 creation sites in the
  load-test server.
- **Reading back through a derived cell right after `batch` returns, on a
  thread that is not the only writer.** It is stale 88% of the time with two
  writers.
- **Counting or edge-detecting on a value cell that more than one thread can
  be writing near.** 85% of its values never arrive with two threads.
- **Writes outside a `batch` into anything with a join.**
- **Latency-sensitive writes on the same process as a large tick.** They wait
  for it: 13.5 ms at p99. Nothing in v3.1.12's API avoids this. Keep ticks
  small, by dedup and by fusing, so that the wait is short.
- **Changing a lookup key as a routine operation.** 1.2 µs, twice a value
  change, plus 1.6 µs when the old row's reader is dropped.

## Small fixes to v3.1.12, ranked

Ranked by measured benefit against size. None is implemented on v3.1.12
except the first, which is the branch `dedup-default`.

| # | fix | where | size | measured benefit | risk |
| --- | --- | --- | --- | --- | --- |
| 1 | Drop a value equal to the one the cell holds. | `Cell::notify`, both the eager path and the queued operation | 24 lines in `cell.rs`. Done on `dedup-default` (72066b5, then 7b4e459 for the toolchain). | See the table under "What it saves". | Behavior change. Eight hyphae tests changed expectation. A `Cell<()>` trigger is exempt by rule. See the next section. |
| 2 | Make `batch` return only after its own writes are applied. A close that finds another thread draining waits for the drain to pass its operations. | `scheduler::batch` and `drain` | Small: a generation counter and a wait. Not written. | Stale read-after-write goes from 88% (2 writers) and 99% (8 writers) to 0 by construction. A single writer's consecutive writes to a value cell stop replacing each other, which today loses 85% to 99% of intermediate values. | A writer then waits for the drain in progress, up to one tick (12.5 ms). Code that calls `batch` while holding a lock a subscriber takes would deadlock. |
| 3 | Skip queueing a repeat instead of dropping it at drain time. | `scheduler::enqueue` | Medium, and subtle: a write that arrives while the cell's previous operation is running must not be compared with the value that operation is about to replace. | With fix 1 a repeat still costs its place in the queue: 219 ns per repeating consumer against 28 ns when `.deduped()` is fused before the cell (100 consumers: 21.9 µs against 2.8 µs). | A lost update if the in-flight case is wrong. |
| 4 | Copy the initial snapshot once per subscribe, not per entry clone per subscriber, and deliver a diff to subscribers without per-subscriber work. | `CellMap::subscribe_diffs` | Small. Not written. | Subscribing costs 12 ns per row of the map (118 µs on 10,000 rows). A row write costs 8.5 ns per subscriber. | Low. |
| 5 | Drop a long chain without recursion. | the drop of a pipeline's install state | Small. Not written. | Dropping a 100,000-cell chain overflows an 8 MB stack. Chains in rship are 200 deep, so this is robustness only. | Low. |
| 6 | Tune the wave hand-off: `HYPHAE_WAVE_THRESHOLD` and `HYPHAE_WORKER_Interleaved tick runs were dominated by other load on the machine: 1 threads: tick p50 42.7 [27.7..57.7] n=2 ms with 4 to 24 other cores busy; 4 threads: tick p50 22.7 [18.3..27.1] n=2 ms with 6 to 12 other cores busy; 8 threads: tick p50 53.9 [36.9..70.9] n=2 ms with 14 to 24 other cores busy. No thread count is measurably better. Under the lighter load of the morning the 4-thread tick was 12.5 ms.`. | environment, no code | None. | Not established. Interleaved tick runs were dominated by other load on the machine: 1 threads: tick p50 42.7 [27.7..57.7] n=2 ms with 4 to 24 other cores busy; 4 threads: tick p50 22.7 [18.3..27.1] n=2 ms with 6 to 12 other cores busy; 8 threads: tick p50 53.9 [36.9..70.9] n=2 ms with 14 to 24 other cores busy. No thread count is measurably better. Under the lighter load of the morning the 4-thread tick was 12.5 ms. | None, it is a setting. |

Not small, and so out of scope: one thread drains at a time (the 13.5 ms wait
of an independent write, and four writers at 58% of one), and `no_coalesce`
cells glitching at joins.

The two collection bugs fixed on `hyphae-settle-core` do not exist in
v3.1.12. d44e316 (a key cell kept a stale value when its map was dropped
before a queued publish applied) and 05204bb (a key cell made in a callback
started at the value subscribers had been told, not the stored value) are
bugs in the queued-publish code that dc95164 added on that branch after
v3.1.12. The regression tests of both fixes, written against the common API
as eight probes, all pass on v3.1.12. The same probes fail 5 of 8 on
`hyphae-settle-core` at the commit before d44e316, so they do detect the
bugs. There is nothing to backport. The probes are a permanent test,
`collection_reads`, on the `bench-suite` branch.

## Dedup by default

**Is it safe at joins?** Yes, as v3.1.12 stands. A join does not wait to hear
from each parent. Each parent's callback updates its half of a stored pair
and notifies the join's cell (`traits/operators/join.rs`, lines 36 to 101).
Inside a batch that notification is queued at the cell's height and merged
with any other for the same cell, and the queue drains in height order
(`scheduler.rs`). The join's cell therefore runs once, after every cell below
it, whether one parent notified it or all did. A parent that stays silent
leaves its half as it was. Nothing would need "unchanged" passed as a signal.

Tested on v3.1.12 as built, with `.deduped()` on every cell, and on
`dedup-default`:

| case | join evaluations per write | inputs from different writes | stalled |
| --- | --- | --- | --- |
| widths 2, 8, 64, one parent changes and the rest repeat, in a batch | exactly 1 per join | 0 | no |
| the same, every parent changes | exactly 1 | 0 | no |
| the same, no parent changes | 1 as built, 0 with dedup | 0 | no: the next changing write computes once |
| three diamonds nested on a 2-wide and an 8-wide one | exactly 1 per join | 0 | no |
| 2 writers on an 8-wide and 4 writers on a 64-wide diamond, nested 2 deep, 20,000 writes each | fewer than 1, writes merge | 0 | no: every sink ends on the last values |
| any of these outside a batch | 2 at width 2, 64 at width 64 | in most writes (4,000 of 6,000 at width 2) | no |

Outside a batch a join already sees mixed inputs on v3.1.12, and dedup
neither causes nor cures that.

**The change.** Branch `dedup-default`, one commit on v3.1.12.
`Cell::notify` compares the incoming value with the held one by `PartialEq`
and returns if they are equal. Inside a batch the comparison runs in the
queued operation, after merging, against the value held then. The rule is
uniform and there is no opt-out. A `no_coalesce` cell drops a repeated
payload too.

**Cells that need to emit equal values, from hyphae's own tests.**

- `Cell<()>` used as a trigger. `sample`'s test writes `()` to its ticker and
  hangs forever if that is dropped. The branch handles it with a rule and not
  an API: a type with a single value always emits, because a write to it can
  only be a signal.
- `state_transition`. Two tests assert one emission per transition when
  consecutive handlers return the same result. With uniform dedup the second
  is dropped, and the tests now say so. If a consumer needs one emission per
  transition it has to put something that changes into the result.

**Every test whose expectation changed (8), and why.**

| test | why |
| --- | --- |
| `stress_high_frequency_updates`, `stress_deep_chain`, `stress_wide_fan_out`, `barrage_extreme_fan_out` | Each counts one emission per write and writes 0 first to a cell that starts at 0. They expect one fewer. |
| `test_indexed_registry_survives_subscription_churn` | The same. It now writes from 1. |
| `no_op_is_dropped_or_stranded_under_sustained_contention` | A `no_coalesce` cell that starts at 0 and is written 800,000 distinct values, one of them 0. It now starts at a value no thread writes. |
| `test_state_transition_valid`, `test_state_transition_selective_emit` | Two handler results in a row are equal. They expect the repeat to be dropped. |

The full hyphae suite ran on the branch with all features: 559 passed and
that `no_coalesce` test failed. After its fix it passed three times alone.
The whole suite was not run again after that one-line test change. Clippy
with `-D warnings`, the build without features and the wasm check pass.

**What it saves.** Only work below a repeated value. Where every value
changes, as in the suite's plain tick, it saves nothing.

Interleaved runs, 3 of v3.1.12 and 2 of `dedup-default` at 7b4e459, both built with Rust 1.100.0, with 6 to 25 cores busy in other processes.

| workload, as built | v3.1.12 | dedup-default | v3.1.12 with `.deduped()` fused at the repeating stage |
| --- | --- | --- | --- |
| 3,600-stream tick, one stage repeats 7 ticks in 8, ms | 16.2 [12.3..54.3] n=3 | 7.9 [6.1..9.6] n=2 | 6.5 [4.1..8.2] n=3 |
| 100 consumers testing one source, µs per write | 83.9 [82.6..114.8] n=3 | 27.3 [19.0..35.6] n=2 | 2.9 [2.8..4.5] n=3 |
| 1,000 consumers testing one source, ms per write | 2.8 [1.9..5.3] n=3 | 0.697 [0.555..0.839] n=2 | 0.020 [0.019..0.038] n=3 |
| chain of 1,000, every value changes, ns per cell | 188 [160..200] n=3 | 178 [171..184] n=2 |  |
| source with 100 derived cells, every value changes, ns per cell | 235 [205..259] n=3 | 251 [215..287] n=2 |  |

## The measurements

Median [minimum..maximum] over the runs, and the run count.

### Build, hold and drop (ns and bytes per item)

| primitive | build ns | drop ns | resident bytes |
| --- | --- | --- | --- |
| `Cell::new` | 350 [323..396] n=5 | 465 [427..528] n=5 | 425 [424..426] n=5 |
| `map` + `materialize` | 1,282 [1,153..1,355] n=5 | 402 [348..485] n=5 | 1,226 [1,224..1,228] n=5 |
| `join` + `map` + `materialize` | 3,963 [3,520..4,197] n=5 | 1,255 [1,069..1,558] n=5 | 3,016 [3,016..3,018] n=5 |
| `subscribe` on a cell | 349 [331..355] n=5 | 101 [87..125] n=5 | 224 [224..224] n=5 |
| ten `map`s fused in one `materialize` | 15,542 [13,901..16,159] n=5 | not run | not measured |
| `map.get(k).materialize()`, 1,000 readers on the map | 211 [170..321] n=5 | 34.5 [18.0..70.0] n=5 | not measured |
| the same, 10,000 readers | 268 [166..381] n=5 | 65.9 [29.4..91.2] n=5 | not measured |
| the same, 100,000 readers | 253 [190..465] n=5 | 68.7 [49.3..96.6] n=5 | not measured |
| keyed lookup (`switch_map` over `map.get`) | 1,914 [1,775..2,373] n=5 | 1,591 [1,355..1,859] n=5 | not measured |
| `subscribe_diffs`, map of 100 rows | 3,589 [3,328..4,073] n=5 | 402 [394..622] n=5 | not measured |
| `subscribe_diffs`, map of 10,000 rows | 118,299 [110,001..120,201] n=5 | 688 [547..859] n=5 | not measured |
| row insert, one write each | 587 [531..639] n=5 |  |  |
| row insert, `insert_many` | 40.5 [34.3..57.6] n=5 |  |  |

### One write through a chain (ns per cell)

| cells in the chain | in a batch | unbatched |
| --- | --- | --- |
| 1 | 273 [247..276] n=5 | 98.4 [94.0..103.1] n=5 |
| 10 | 149 [136..156] n=5 | 62.8 [56.5..63.7] n=5 |
| 100 | 151 [137..165] n=5 | 77.2 [76.1..82.1] n=5 |
| 1,000 | 156 [147..158] n=5 | 87.2 [83.9..90.7] n=5 |
| 100,000 | not run | not run (recursion) |
| 10 maps fused in one cell (ns per write) | 332 [306..338] n=5 |  |

### One write to a source with N direct dependents (ns per dependent)

| N | derived cells, in a batch, 4 worker threads | derived cells, in a batch, 1 worker thread | derived cells, unbatched | plain subscribers, in a batch |
| --- | --- | --- | --- | --- |
| 1 | 256 [232..263] n=5 | 269 [269..269] n=1 | 87.9 [82.5..91.4] n=5 | 136 [130..142] n=5 |
| 10 | 137 [122..153] n=5 | 149 [149..149] n=1 | 44.9 [41.1..46.9] n=5 | 14.4 [13.5..16.9] n=5 |
| 100 | 202 [188..210] n=5 | 226 [226..226] n=1 | 48.0 [45.4..49.4] n=5 | 3.79 [3.61..4.08] n=5 |
| 1,000 | 750 [569..955] n=5 | 402 [402..402] n=1 | 62.9 [60.6..87.4] n=5 | 3.75 [3.71..4.52] n=5 |
| 10,000 | 868 [614..1,446] n=5 | 421 [421..421] n=1 | 89.1 [78.9..606.0] n=5 | 4.57 [4.44..6.43] n=5 |
| 100,000 | 1,701 [1,313..1,863] n=5 | 1,618 [1,618..1,618] n=1 | 512 [432..565] n=5 | 5.51 [5.33..25.28] n=5 |

### Fixed costs and single operators (ns per write)

| operation | ns |
| --- | --- |
| empty `batch` | 7.16 [6.50..7.42] n=5 |
| `set` on a cell nothing reads, in a batch | 117 [105..121] n=5 |
| `set` on a cell nothing reads, unbatched | 30.9 [29.3..31.6] n=5 |
| write one side of a `join` with one subscriber | 399 [393..405] n=5 |
| diamond (source, two maps, join, subscriber), in a batch | 665 [627..699] n=5 |
| diamond, unbatched | 371 [368..403] n=5 |
| keyed lookup: write the row it reads | 657 [648..684] n=5 |
| keyed lookup: change its key | 1,233 [1,179..1,241] n=5 |
| keyed lookup: write another row of the table | 156 [148..162] n=5 |

### One row write to a CellMap (ns per write)

| what reads the map | ns per write |
| --- | --- |
| nothing, 1,000 rows | 127 [123..134] n=5 |
| nothing, 100,000 rows | 128 [126..139] n=5 |
| 1,000 `get(k)` readers of other keys | 148 [140..150] n=5 |
| 10,000 `get(k)` readers of other keys | 145 [142..158] n=5 |
| 100,000 `get(k)` readers of other keys | 139 [135..144] n=5 |
| 10 `subscribe_diffs` subscribers | 194 [194..203] n=5 |
| 100 `subscribe_diffs` subscribers | 873 [826..879] n=5 |
| 1,000 `subscribe_diffs` subscribers | 9,020 [8,780..9,727] n=5 |
| 10,000 `subscribe_diffs` subscribers | 84,769 [72,497..101,650] n=5 |
| a `map_values` view, 1,000 rows | 291 [285..340] n=5 |
| a `map_values` view, 10,000 rows | 289 [280..307] n=5 |
| a `len()` reader, 10,000 rows | 129 [123..131] n=5 |
| an `entries()` reader, 1,000 rows | 704 [662..793] n=5 |
| an `entries()` reader, 10,000 rows | 3,885 [3,577..4,091] n=5 |

### Callbacks per write of an equal value (v3.1.12, and with dedup by default)

| write | v3.1.12 | dedup-default |
| --- | --- | --- |
| `Cell::set` of the value it holds | 1.00 [1.00..1.00] n=5 | 0.00 [0.00..0.00] n=3 |
| `map` whose result did not change | 1.00 [1.00..1.00] n=5 | 0.00 [0.00..0.00] n=3 |
| the same with `.deduped()` | 0.00 [0.00..0.00] n=5 | 0.00 [0.00..0.00] n=3 |
| `join` whose result did not change | 1.00 [1.00..1.00] n=5 | 0.00 [0.00..0.00] n=3 |
| keyed lookup, key moves to a row with the same value | 1.00 [1.00..1.00] n=5 | 0.00 [0.00..0.00] n=3 |
| `CellMap::insert` of the value the row holds: `get(k)` reader | 0.00 [0.00..0.00] n=5 | 0.00 [0.00..0.00] n=3 |
| the same: `subscribe_diffs` | 0.00 [0.00..0.00] n=5 | 0.00 [0.00..0.00] n=3 |
| `map_values` whose output row did not change: diffs | 0.00 [0.00..0.00] n=2 | 0.00 [0.00..0.00] n=3 |

### What dedup saves (microseconds per write)

| workload | v3.1.12 as built | v3.1.12 with `.deduped()` at the repeating stage | dedup-default, as built |
| --- | --- | --- | --- |
| 100 consumers each testing one source | 85.5 [82.5..90.1] n=5 | 2.84 [2.70..2.93] n=5 | 21.9 [18.6..22.1] n=3 |
| 10,000 consumers each testing one source | 29,035 [24,624..39,607] n=5 | 301 [262..460] n=5 | 12,205 [8,055..13,852] n=3 |
| 100,000 consumers each testing one source | 383,639 [287,845..537,349] n=5 | 18,353 [5,496..22,752] n=5 | 127,849 [122,543..143,496] n=3 |
| 3,600-stream tick, one stage repeats 7 ticks in 8 | 10,852 [9,300..18,248] n=5 | 4,229 [3,309..4,863] n=5 | 6,169 [6,133..7,379] n=3 |

### Writers that contend: each thread writes its own cell, one batch per write

| threads | value cells: writes the sink never saw | value cells: read-after-write stale | event cells: writes never seen | event cells: read-after-write stale | event cells: writes per second, all threads |
| --- | --- | --- | --- | --- | --- |
| 1 | 0.00 [0.00..0.00] n=5 | 0.00 [0.00..0.00] n=5 | 0.00 [0.00..0.00] n=5 | 0.00 [0.00..0.00] n=5 | 3,555,158 [3,521,542..4,166,385] n=5 |
| 2 | 0.85 [0.74..0.92] n=5 | 0.88 [0.76..0.95] n=5 | 0.00 [0.00..0.00] n=5 | 0.50 [0.50..0.50] n=5 | 3,376,912 [3,110,086..3,782,194] n=5 |
| 4 | 0.96 [0.90..0.98] n=5 | 0.97 [0.91..0.99] n=5 | 0.00 [0.00..0.00] n=5 | 0.75 [0.75..0.75] n=5 | 3,253,396 [2,453,737..3,443,537] n=5 |
| 8 | 0.99 [0.96..1.00] n=5 | 0.99 [0.96..1.00] n=5 | 0.00 [0.00..0.00] n=5 | 0.88 [0.87..0.88] n=5 | 1,676,208 [1,550,049..1,934,842] n=5 |

### At rship scale (scale 1: 246,000 cells), microseconds unless said

| quantity | value |
| --- | --- |
| build the whole graph, ms | 405 [393..451] n=5 |
| resident memory of the graph, MB | 333 [333..333] n=5 |
| tick of 3,600 streams, about 25,000 cells, p50 | 12,452 [11,988..18,794] n=5 |
| the same, p99 | 25,424 [21,833..28,115] n=5 |
| the same on 1 worker thread, p50 | 31,629 [30,708..32,549] n=2 |
| the same on 8 worker threads, p50 | 32,121 [32,121..32,121] n=1 |
| the same with 10 times the untouched state, p50 | 56,573 [56,573..56,573] n=1 |
| tick at scale 2 (50,000 cells), p50 | 29,675 [26,599..30,922] n=5 |
| tick at scale 4 (100,000 cells), p50 | 68,015 [66,217..85,126] n=5 |
| one trigger to 5,000 chains of 5, p50 | 13,153 [11,852..21,145] n=5 |
| independent 5-cell write, idle, p50 | 2.92 [2.33..4.85] n=5 |
| the same, idle, p99 | 34.7 [25.8..39.4] n=5 |
| the same while the tick runs, p50 | 11.2 [2.5..20.7] n=5 |
| the same while the tick runs, p99 | 13,481 [10,452..15,038] n=5 |
| write inside the tick's scene while it runs, p99 | 15,759 [14,954..35,962] n=5 |
| store row write (45,000 keyed readers), idle, p99 | 13.4 [12.1..34.3] n=5 |
| the same while the tick runs, p99 | 5,216 [3,870..8,459] n=5 |
| cells evaluated per store row write | 2.00 [2.00..2.00] n=5 |
| one 200-cell chain, p50 | 77.6 [55.8..97.9] n=5 |
| 40 chains of 200 in one write, p50 | 1,935 [1,725..5,990] n=5 |
| feedback: ns per callback write, p50 | 114 [114..118] n=5 |
| lookup key change, p50 | 12.9 [11.9..20.9] n=5 |
| value change in the same scene, p50 | 7.63 [6.48..12.00] n=5 |
| add a keyed pipeline with a subscriber and drop the oldest, p50 | 6.12 [5.66..9.91] n=5 |
| add a 7-cell stream to a live scene, p50 | 30.4 [29.2..40.8] n=5 |
| 64-cell write, one writer, p50 | 33.8 [32.9..36.3] n=5 |
| 64-cell write, four writers at once, p99 | 337 [281..769] n=5 |
| closed-loop throughput, 1 writer, writes/s | 95,401 [66,086..99,495] n=5 |
| closed-loop throughput, 4 writers, writes/s | 56,205 [29,373..57,141] n=5 |
| ticks some sink missed during overload (of 3,600 x 165) | 140,400 [133,200..162,000] n=5 |
| ticks some sink missed during churn | 0.00 [0.00..7200.00] n=5 |
| pulses missed of a 1 kHz train during the tick (event cells) | 0.00 [0.00..0.00] n=5 |
| tick p99 during overload | 37,540 [35,056..39,096] n=5 |
| independent write p99 during overload | 87,490 [68,771..94,523] n=5 |
| time to drain after overload, ms | 67.3 [5.6..134.8] n=5 |
