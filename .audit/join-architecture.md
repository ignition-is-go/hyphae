# Join remediation architecture

## Decision

Use one private lifecycle host with two execution kernels. The arbitrary-N region and the specialized two-stage join will share state ownership, callback transactions, publication, root-order policy, and guard collection. They will retain separate routing algorithms.

Candidate A is the base design. Candidate C contributes the safer migration rule: keep the current `TwoLeftJoinPlan` and `TwoLeftJoinMappedPlan` field layout and third-join lowering until a separate type-correct replacement proves drop order and auto-trait parity. Candidate B contributes named serial and sharded two-stage state and focused promotion tests.

## Context

The current implementations duplicate lifecycle policy but do not implement the same algorithm:

- the two-stage engine routes by each stage's join key, migrates intermediate keys, registers left then rights, and has a measured 2.6201 microsecond single-update path;
- the arbitrary-N engine routes a typed stage spine by the original map key, registers rights then left, maintains shared relationship indexes, and uses fail-stop query poisoning;
- `RegionRouter` represents serial and sharded ownership with two `Option` fields and stores `parallel_active` independently, so impossible execution states are representable;
- the public two-stage build accepts the weaker `L: MapQuery` bound, while `JoinRegion` also requires `PlanProperties<OutputPartition = ByMapKey<K>>`.

Directly delegating the two-stage plan to `JoinRegion` would therefore change bounds, routing, root order, panic behavior, batch publication, or performance. It is rejected for this update.

## Private shape

Add an ownership-focused internal module with an exhaustive runtime state:

```rust
enum RuntimeStorage<Serial, Sharded> {
    Serial(Serial),
    Sharded {
        runtime: Sharded,
        parallel_active: bool,
    },
}
```

The lifecycle host owns the mutex, sink, transaction policy, and callback dispatch. Dispatch order is always lock, apply, unlock, publish. Transaction policy is selected by type so the measured two-stage path does not gain a runtime policy branch.

The host accepts an explicit root registration order:

- `LeftThenRights` for the specialized two-stage path;
- `RightsThenLeft` for the arbitrary-N region.

Each kernel returns its observable `MapDiff` shape. The lifecycle layer does not normalize or flatten output.

The arbitrary-N router retains typed stages, maintaining-shard-first right updates, repeated-key event ordering, source ordering, and fail-stop query poison. The specialized kernel retains the atomic-left shortcut, two keyed states, stage-key sharding, intermediate-key migration, and existing promotion thresholds.

## Public compatibility

This update must not change the names, generic arity, bounds, methods, or return types of `LeftJoinPlan`, `TwoLeftJoinPlan`, `TwoLeftJoinMappedPlan`, `JoinRegion`, `JoinStage`, `JCons`, or `JNil`.

The current two-stage plan field layout and third-join reconstruction remain during the first migration. The existing private installer signature may remain as the narrow adapter while its closure tower moves behind a stateful kernel. No new public exports or dependencies are allowed.

## Migration

1. Add canonical `MapDiff` traversal only for semantics proven identical by differential tests.
2. Add `RuntimeStorage` with tests for serial access, successful promotion, repeated promotion, and panic during promotion construction.
3. Migrate `RegionRouter` to the exhaustive storage enum and split serial and sharded application helpers without changing lifecycle wiring.
4. Move region callback transactions behind the lifecycle host while preserving fail-stop behavior and root order.
5. Extract named `TwoKeyedSerial`, `TwoKeyedSharded`, and `TwoStageKernel` types from the specialized installer.
6. Move the specialized callbacks behind the monomorphized lifecycle host. Delete the old orchestration in the same unit.
7. Keep the current plan lowering and public declarations. Consider deeper plan-representation deletion only in a separately proven change.

Each step ends with focused behavior tests. The specialized-shell step also ends with a benchmark comparison; if it regresses, revert only that shell migration while retaining the exhaustive state and extracted kernel.

## Release gates

- `cargo semver-checks` against `4465c6dbe364bfee9f97136d64420549af14d800` reports no breaking change.
- External fixtures continue to name and compose the public plan types, including three- and eight-stage joins.
- Exact behavior covers initial state, insert, update, remove, nested batch, repeated-key batch, rekey, every root, synchronous settlement, poison, and guard retention in serial and promoted modes.
- Root registration order, physical-root interning, and shared-index exact-once maintenance remain unchanged.
- The two-stage confidence interval overlaps the 2.6117 to 2.6282 microsecond baseline or its point estimate is no more than five percent slower on the same machine.
- Recurring update allocations do not increase.

## Rejected alternatives

Full `JoinRegion` delegation is smaller but changes load-bearing bounds and algorithms. A shared configurable routing planner would encode the algorithm differences as options and increase reader load. Pure helper extraction alone fixes the state smell but leaves lifecycle policy duplicated.

## Arena record

The three independent candidates are in `/tmp/hyphae-architect/candidate-{a,b,c}/design.md`. The cross-judge report is `/tmp/hyphae-architect/judge.md`. The judge scored Candidate A highest at 9.25/10 and required the conservative public-plan migration recorded above.
