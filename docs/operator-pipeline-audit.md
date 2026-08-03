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
| `join` | `JoinPipeline` with two install-local latest values | removed |
| `merge` | `MergePipeline` with install-local completion state | removed |
| `zip` | `ZipPipeline` with install-local pair queues | removed |
| `join_vec` | `JoinVecPipeline` with install-local latest values | removed |
| `take_until` | `TakeUntilPipeline` with two root subscriptions | removed |

`drop_oldest` and `sample_latest` are identities because delivery is
synchronous and a materialized cell already stores the latest value.
`drop_oldest`'s former queue never affected its emitted stream.

After this pass, 5 operator entry points still return concrete cells. Every one
of them dynamically replaces or adds a subscription after installation.

## Remaining single-source operators

These can be direct pipeline nodes. Their mutable state should be created by
`PipelineInstall::install`, so merely constructing or chaining the operator
does not allocate state or subscribe.

| Operators | Install-local state |
| --- | --- |

The time-based nodes must capture the pipeline callback, not a weak reference
to a privately allocated output cell. Their existing generation/cancellation
logic still applies.

## Remaining multi-source operators

| Operators | Additional requirement |
| --- | --- |
| `concat` | completion changes which root subscription is active |
| `switch_map`, `merge_map` | dynamic inner-subscription ownership |
| `retry`, `retry_when` | dynamic re-subscription to the source |

Static multi-root ownership is provided by `SubscriptionGuard::combine`, whose
composite `DepNode` exposes every root and forwards scheduler invalidation
registration. The remaining dynamic operators need the corresponding mutable
RAII subscription slot, including dependency replacement and height-cone
invalidation, owned by the installed pipeline rather than by an output cell.

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
