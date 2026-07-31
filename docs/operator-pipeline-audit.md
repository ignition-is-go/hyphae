# Operator Pipeline Audit

The design rule is that an operator describes a computation and returns a
`Pipeline`. A `Cell` is a cached, multicast observation boundary and should be
allocated only when a caller explicitly asks to `materialize`.

This supersedes the original pipeline migration assumption that stateful
operators must return cells. Operator state can belong to an installed pipeline
subscription; state alone does not require a cell.

## Current result

The first pass removes four avoidable cell boundaries:

| Operator | Pipeline shape | Internal cell |
| --- | --- | --- |
| `window` | existing typed `scan -> map` chain | removed |
| `buffer_count` | `BufferCountPipeline` with install-local queue | removed |
| `drop_oldest` | identity pipeline | removed |
| `sample_latest` | identity pipeline | removed |
| `drop_newest` | `DropNewestPipeline` with install-local queue | removed |

`drop_oldest` and `sample_latest` are identities because delivery is
synchronous and a materialized cell already stores the latest value.
`drop_oldest`'s former queue never affected its emitted stream.

After this pass, 17 operator entry points still return concrete cells.

## Remaining single-source operators

These can be direct pipeline nodes. Their mutable state should be created by
`PipelineInstall::install`, so merely constructing or chaining the operator
does not allocate state or subscribe.

| Operators | Install-local state |
| --- | --- |
| `state_transition` | current state-machine state |
| `audit` | latest value, generation, window flag |
| `buffer_time` | pending chunk, timer generation |
| `debounce` | generation |
| `delay` | delayed jobs |
| `throttle` | last-emission time |
| `timeout` | generation, timed-out flag |

The time-based nodes must capture the pipeline callback, not a weak reference
to a privately allocated output cell. Their existing generation/cancellation
logic still applies.

## Remaining multi-source operators

| Operators | Additional requirement |
| --- | --- |
| `join`, `merge`, `zip`, `join_vec` | one installed pipeline owns multiple root guards |
| `concat`, `take_until` | completion can change which root subscription is active |
| `switch_map`, `merge_map` | dynamic inner-subscription ownership |
| `retry`, `retry_when` | dynamic re-subscription to the source |

Before porting these, subscription ownership needs a composite guard whose
`DepNode` exposes every live upstream dependency. A callback-only guard would
unsubscribe correctly but would hide the dependency graph from scheduler
height calculation. Dynamic operators additionally need an RAII subscription
slot owned by the installed pipeline rather than by an output cell.

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
