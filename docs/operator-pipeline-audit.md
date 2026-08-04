# Operator Pipeline Audit

The design rule is that an operator describes a computation and returns a
`Pipeline`. A `Cell` is a cached, multicast observation boundary and should be
allocated only when a caller explicitly asks to `materialize`.

This supersedes the original pipeline migration assumption that stateful
operators must return cells. Operator state can belong to an installed pipeline
subscription; state alone does not require a cell.

## Current result

The migration has removed every static single- and multi-source cell boundary:

| Operator | Pipeline shape | Internal cell |
| --- | --- | --- |
| `audit` | `AuditPipeline` with install-local window state | removed |
| `window` | existing typed `scan -> map` chain | removed |
| `buffer_count` | `BufferCountPipeline` with install-local queue | removed |
| `buffer_time` | `BufferTimePipeline` with install-local queue/timer | removed |
| `debounce` | `DebouncePipeline` with install-local generation | removed |
| `delay` | `DelayPipeline` with install-local timer callbacks | removed |
| `drop_oldest` | identity pipeline | removed |
| `sample_latest` | identity pipeline | removed |
| `state_transition` | `StateTransitionPipeline` with install-local state | removed |
| `drop_newest` | `DropNewestPipeline` with install-local queue | removed |
| `throttle` | `ThrottlePipeline` with install-local gate | removed |
| `timeout` | `TimeoutPipeline` with install-local generation | removed |
| `join` | `JoinPipeline`; install-time coalescing cell preserves glitch-free diamonds | required |
| `merge` | `MergePipeline` with install-local completion state | removed |
| `zip` | `ZipPipeline` with install-local pair queues | removed |
| `join_vec` | `JoinVecPipeline`; install-time coalescing cell preserves glitch-free fan-in | required |
| `take_until` | `TakeUntilPipeline` with two root subscriptions | removed |
| `concat` | `ConcatPipeline` with install-time handoff owner | required |
| `switch_map` | `SwitchMapPipeline` with install-time keyed inner owner | required |
| `merge_map` | `MergeMapPipeline` with install-time accumulating inner owner | required |
| `retry`, `retry_when` | `RetryPipeline` with install-time keyed retry owner | required |

`drop_oldest` and `sample_latest` are identities because delivery is
synchronous and a materialized cell already stores the latest value.
`drop_oldest`'s former queue never affected its emitted stream.

No operator entry point now returns a concrete cell. `concat`, `switch_map`,
`merge_map`, `retry`, and `retry_when` return typed pipelines and create their
dynamic subscription owner only when installed.

Public derived-view entry points follow the same type-level contract even when
their current implementation uses a cell internally. `CellMap::{get, entries,
items, keys, size, len, diffs}`, `CellSet::{contains, values, len, diffs}`, and
`Source::sample_on` expose definite pipelines, so consumers must choose the
observation boundary explicitly. This prevents callers from depending on the
present cache shape and lets those implementations become deferred without a
future return-type break. Explicit boundaries and hot sources (`materialize`,
`to_cell`, `lock`, `interval`) still return cells directly.

`switch_map` and `merge_map` accept inner pipelines directly. Generated inner
chains therefore stay unmaterialized and fuse down to the dynamic ownership
boundary instead of allocating one cell per inner recipe.

Static multi-root ownership is provided by `SubscriptionGuard::combine`, whose
composite `DepNode` exposes every root and forwards scheduler invalidation
registration. Dynamic operators need the corresponding mutable
RAII subscription slot, including dependency replacement and height-cone
invalidation. Dynamic operators use an install-time cell for this job because
`Cell::own` and `Cell::own_keyed` are the scheduler-aware mutable subscription
slots: they update dependency heights and guarantee teardown on replacement.

`join` and `join_vec` are the deliberate exceptions to the no-internal-cell
rule. Their fan-in must coalesce sibling updates before a fused downstream
operator runs; otherwise one batched diamond executes downstream work once per
root and exposes torn intermediate states. They remain lazy pipeline values,
and allocate that boundary only when installed.

## Required invariants for every port

1. Constructing and chaining the operator adds no root subscription.
2. Materializing the whole chain adds only the subscriptions required by its
   roots, with no intermediate cell.
3. Each installation gets independent mutable state.
4. Initial replay and `PipelineSeed` agree, so materialization does not
   duplicate or lose the seed emission.
5. Completion and error propagation match the existing operator.
6. Scheduler dependency heights include every current upstream root.
7. Dropping the materialized output tears down timers and all static or dynamic
   subscriptions.
