# Repeated values do not propagate

A cell drops a value equal to the one it holds. The value is not stored again,
no subscriber is called, and nothing derived from the cell recomputes. This
holds for a source written with `set`, and for every cell `materialize`
creates: a `map` whose result did not change is silent, and so is a `join`
whose pair did not change.

The comparison is `PartialEq` on the value type. It runs in `Cell::notify`.
Inside a `batch` it runs when the scheduler applies the write, after writes
to the same cell have been merged, against the value the cell holds then.
Completion and errors always pass. There is no opt-out.

## Events

An event that must be seen every time carries something that differs every
time, such as a tick or a sequence number. Then no two emissions are equal
and each passes. A pulse that repeats the payload the cell holds, with
nothing else to tell it apart, is dropped. That holds for a `no_coalesce`
cell too: `no_coalesce` keeps every write to the cell as its own queued
operation, and each is still compared with the value the cell holds when it
runs.

One exception is built in. A cell whose type has a single value, such as
`Cell<()>`, always emits. A write cannot change it, so the write itself is
the signal. `sample` takes such a cell as its ticker.

## Joins and diamonds

A join does not wait to hear from each parent. Inside a batch the scheduler
runs cells in height order and a join's cell runs once, after every cell below
it, with whatever its parents now hold. A parent that stays silent leaves its
half of the pair as it was. `hyphae/tests/dedup_diamonds.rs` pins this at
widths 2, 8 and 64, three diamonds deep, with one parent silent, with all
parents silent, and with two and four concurrent writers: the join computes
once per write on inputs that reflect the same write, and computes nothing
when no parent changed.

Outside a batch nothing has changed: propagation is eager, a join computes
once per parent that emits, and it can see parents from different writes.

## What changes for existing code

- A write of the value a cell already holds is no longer observed. Code that
  counts emissions, or uses a write of an unchanged value as a trigger, must
  put something that changes into the value.
- An operator's output is a cell, so an operator that emits two equal results
  in a row emits one. The tests show it for `state_transition`: two
  transitions whose handlers return the same result emit once. By the same
  construction it applies to `sample`, `scan`, `merge`, `zip`, `pairwise`,
  `buffer_count`, `buffer_time`, `window` and `with_latest_from`; no test
  here covers those.
- A value type whose `PartialEq` ignores a field hides changes to that field.

Eight tests changed with this behavior. One started a `no_coalesce` cell at
0 and counted every write, one of which was 0
(`no_op_is_dropped_or_stranded_under_sustained_contention`); it now starts
the cell at a value no thread writes. Five counted one emission per write
and wrote the initial value first (`stress_high_frequency_updates`,
`stress_deep_chain`, `stress_wide_fan_out`, `barrage_extreme_fan_out`,
`test_indexed_registry_survives_subscription_churn`); they now expect one
fewer, or start from 1. Two asserted that `state_transition` emits once per
transition when consecutive handlers return the same result
(`test_state_transition_valid`, `test_state_transition_selective_emit`); they
now expect the repeat to be dropped.
