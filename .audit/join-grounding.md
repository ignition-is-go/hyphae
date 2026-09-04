# Join architecture grounding

## Public caller flow

`LeftJoinExt::left_join_by` creates `LeftJoinPlan`. A key-preserving projection followed by another join produces the public `TwoLeftJoinPlan`; mapping its last stage produces `TwoLeftJoinMappedPlan`. The mapped two-stage plan's third `left_join_by` consumes those wrappers and returns the existing public `JoinRegion<L, JCons<...>, LK, LV>` shape.

Materialization creates a `CompileContext`, connects the complete plan, and activates physical roots after connection. The specialized two-stage `BuildQueryRuntime` implementations call `install_two_keyed_join_runtime_via_query`. The arbitrary-N path constructs a typed runtime spine, registers right roots before the left root so indexes are seeded, and routes every callback through one `RegionRouter` transaction.

## Compatibility constraints

- Preserve the names, generic arity, methods, bounds, and return types of `LeftJoinPlan`, `TwoLeftJoinPlan`, `TwoLeftJoinMappedPlan`, `JoinRegion`, `JoinStage`, `JCons`, and `JNil`.
- Preserve the exact third-join return type. Downstream callers can name these public types even when they are hidden from documentation.
- Do not strengthen the legacy `BuildQueryRuntime` bounds. `JoinRegion` requires a `ByMapKey<K>` left partition while the legacy implementation accepts a weaker `L: MapQuery` bound.
- Preserve root registration order, physical-root interning, right-match insertion order, left-source output order, repeated-key event order, batch shape, synchronous settlement, rekeying, and guard retention.
- Treat two-stage performance as a gate. The specialized engine routes by each stage's join key, while the region engine pins a map key to a complete heterogeneous spine.

## Runtime state

`RegionRouter` is a one-way machine with serial storage followed by sharded storage. `parallel_active` selects processing for already-sharded storage; it is not a third storage mode. `poisoned` is terminal and coordinates with query-wide poison.

The current `Option<Runtime>` plus `Option<Vec<Runtime>>` permits both-or-neither states. A private `RuntimeStorage<R> { Serial(R), Sharded(Vec<R>) }` encodes the real ownership without touching public APIs.

The left path separately decides batch semantics, promotion, parallel scheduling, routing, and output order. Repeated-key batches must execute eventwise. The right path advances flattened right events one at a time so all observer shards see the maintaining shard's index snapshot.

## Specialized-engine difference

The specialized engine owns two separate join states and, after promotion, two `ShardedKeyedJoin` values. It routes by stage join keys and handles intermediate-key migration. The region engine routes by original map key and keeps one complete stage spine per shard. Direct delegation may change performance, bounds, failure behavior, or publication order.

## Design question for the arena

Find the smallest non-breaking architecture that removes duplicate lifecycle decisions without erasing the proven two-stage kernel. Candidate designs must compare full region delegation, a common transaction/install shell with separate kernels, and shared pure planning/state components. The design must state what can be deleted now, what remains specialized, and the evidence required before further consolidation.

## Evidence

- `hyphae/src/traits/collections/left_join.rs:607-1123,1127-1386`
- `hyphae/src/traits/collections/internal/join_runtime.rs:16-2128`
- `hyphae/src/traits/collections/internal/join_region.rs:502-551,633-1300,1462-2025,2074-2478`
- `hyphae/src/map_query/mod.rs:148-260`
- `hyphae/src/map_query/compiler.rs:265-340`
- `hyphae/tests/arbitrary_n_public_join_region.rs:22-124`
- graph `hyphae-current`, generation `2026-09-04T06:40:43Z`, full index, clean coverage for all cited Rust files
