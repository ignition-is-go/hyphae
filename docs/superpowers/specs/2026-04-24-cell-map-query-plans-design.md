# CellMap Query Plans Design

**Date:** 2026-04-24
**Status:** Approved (brainstorm); ready for implementation plan
**Related work:** Pipeline refactor (`feat/pipelines` branch) introduced an analogous lazy-plan / `materialize()` pattern for single-cell pure operators.

## Goal

Reduce contention and atomic overhead when chaining `CellMap` operations (joins, projections, filters). Today, every step in a chain like
`a.left_join_by(&b, ...).left_join_by(&c, ...).project(...)` allocates its own `CellMap`, with its own subscriber table, broadcaster, and `ArcSwap` per indexed entry. Three chained joins = three full sets of reactive machinery, three `ArcSwap` stores per change, three subscriber notifications.

After this refactor, the same chain materializes to **one** output `CellMap` with **one** set of subscriber machinery, **one** `ArcSwap` per change, and **one** install of subscriptions on each root source.

## Non-Goals (Phase B Material)

- **Index dedup across stages.** Two joins keying on the same field today maintain separate hash indexes; sharing them is deferred. The plan-tree structure leaves room for an `optimize(plan) -> plan` rewrite pass to add this later.
- **Projection pushdown.** Carrying only fields the consumer reads through each stage is deferred.
- **Join reordering / cardinality stats.** Static plan execution; no statistics-driven decisions.

## Architecture

### `MapQuery<K, V>` trait

Analogous to `Pipeline<T>` but for keyed reactive collections.

- Not `Clone`. Cloning a plan would silently duplicate the join/projection work; users must `materialize()` to get a `Clone`-able `CellMap`.
- No `subscribe`. Same reasoning as `Pipeline` — subscribing a plan would force memoization at an arbitrary point. Materialize first.
- One method: `fn materialize(self) -> CellMap<K, V, CellImmutable>`.
- Crate-private installer hook (mirrors `PipelineInstall`): `MapQueryInstall<K, V>` — used by `materialize()` to wire up root-source subscriptions.

### Blanket implementation

`impl<M: ReactiveMap> MapQuery<M::Key, M::Value> for M` so any reactive map (currently `CellMap` and any future `ReactiveMap` impls) is itself a query — making `cell_map.materialize()` a no-op identity (returns a locked clone) and letting the trait act as the input bound for all operators.

### Plan node structs

One per existing operator. Mirrors the per-operator-port pattern from the Pipeline refactor.

| Operator | Plan node |
|---|---|
| `inner_join`, `inner_join_fk`, `inner_join_by` | `InnerJoinPlan<L, R, ...>` |
| `left_join`, `left_join_by` | `LeftJoinPlan<L, R, ...>` |
| `left_semi_join`, `left_semi_join_by` | `LeftSemiJoinPlan<L, R, ...>` |
| `multi_left_join` | `MultiLeftJoinPlan<...>` |
| `project` | `ProjectPlan<S, ...>` |
| `project_many` | `ProjectManyPlan<S, ...>` |
| `project_cell` | `ProjectCellPlan<S, ...>` |
| `select` | `SelectPlan<S, ...>` |
| `select_cell` | `SelectCellPlan<S, ...>` |
| `count_by` | `CountByPlan<S, ...>` |
| `group_by` | `GroupByPlan<S, ...>` |

Each holds owned references to its source(s) (typed by `MapQuery` parameter), the user's closures (key extractors, projection functions, etc.), and any compile-time-known config (e.g., key type tags). No runtime state, no allocation of `CellMap` until materialize.

### Composability

Every operator method is generic over `M: MapQuery<K, V>`. Both `CellMap` (via blanket) and every plan node implement `MapQuery`, so plans can be passed as inputs to other plans:

```rust
let inner = a.inner_join(b, ...);     // returns InnerJoinPlan; a, b consumed
let outer = inner.left_join(c, ...);  // takes inner by self; c by value
let result = outer.materialize();     // returns CellMap
```

Right-hand-side inputs are taken **by value** (consuming), unlike today's `&R`. Plans aren't `Clone`, so passing a plan as input transfers ownership; the composed plan materializes as one unit, no work duplication. `CellMap` is cheap to clone (`Arc` bump), so passing a `CellMap` reused elsewhere becomes `instances.left_join_by(lanes.clone(), ...)`.

## Materialize execution model

`MapQuery::materialize(self) -> CellMap<K, V, CellImmutable>` walks the plan tree:

1. **Collect root sources** — the underlying reactive maps the plan reads from. A reactive map appearing in the plan multiple times (e.g., self-join) counts as one source with multiple consumers in the fused callback.
2. **Allocate** one mutable output `CellMap<K, V, CellMutable>`.
3. **Install one subscription per root source.** Each subscription's callback runs the fused execution: walk the plan with the change applied, recompute affected output rows, write them into the output map. Per-stage state (join hash indexes, group-by accumulators, scan state) lives in the closure captures.
4. **Own** all subscription guards inside the output map and **lock** to `CellImmutable`.

Per-change cost reduction (chain-of-N-joins example):

| | Today | Post-refactor |
|---|---|---|
| `CellMap` allocations | N | 1 |
| Subscriber tables | N | 1 |
| `ArcSwap` stores per change | N | 1 |
| Broadcaster atomics per change | N | 1 |
| Hash indexes maintained | N | N (each join still needs its own; dedup is phase B) |
| Subscriptions on root sources | linear in chain | one per root source, regardless of chain depth |

## API surface change

| Aspect | Today | Post-refactor |
|---|---|---|
| Method names | `inner_join`, `left_join_by`, `project`, `select`, `count_by`, `group_by`, ... | unchanged |
| Receiver | `&self` | `self` (consume) |
| RHS inputs | `&R` | `R: MapQuery<...>` (consume) |
| Return type | `CellMap<K, V, CellImmutable>` | `*Plan<...>` (the plan node) |
| Consumer wanting a `CellMap` | works directly | insert `.materialize()` |
| Reuse of a source | re-borrow | `.clone()` (cheap on `CellMap`; pass through composition for plans) |

Method names stay the same so the breakage surfaces at consumption sites (not at the call site), exactly as the Pipeline refactor did.

## Migration scope

Internal hyphae callers using these collection methods need `.materialize()` insertion or to be updated to take `MapQuery` rather than `CellMap` directly. Audit pattern mirrors the Pipeline rship audit: roughly mechanical with a small set of non-trivial sites.

External (rship) audit: per the existing `2026-04-23-pipeline-type-rship-audit.md`, rship has many uses of `inner_join_by`, `left_join_by`, `left_semi_join_by`, plus `project`/`select`/`project_cell`/`select_cell` chains. Migration is mechanical (insert `.materialize()` at consumption sites; replace `&map` with `map.clone()` at composition sites). Most damage is one-line per site.

## Test plan

- **Adapt existing collection trait tests** with `.materialize()` insertion. Same mechanical pattern as the Pipeline refactor's port tasks.
- **New integration tests** in `hyphae/src/tests/map_query_integration.rs`:
  - `materialize_collapses_chain` — chain three joins, materialize, assert each root source has exactly one subscriber installed (compared to a baseline measurement before the chain).
  - `compose_plans` — build a plan, use it as input to another plan's join argument, materialize once, verify correct output and that no intermediate `CellMap` was allocated.
  - `plan_not_clone` — compile-fail (or trait-bound) test asserting plan nodes do not implement `Clone`.
  - `materialize_consumes` — confirm the plan is moved into `materialize`.
- **Benchmark** `hyphae/benches/cell_map_chains.rs` analogous to `pipeline_chains.rs`:
  - Pre-refactor baseline saved as `pre-cellmap-query-plans`.
  - Measures throughput of "change one root source, observe propagation" for chain depths 1, 3, 5, 10 of `left_join_by`/`project` mixes.
  - Expected: per-change cost flat in chain depth post-refactor (single `ArcSwap` regardless of N).

## Phase B hooks

The plan tree is the optimization substrate.

- Plan nodes are `pub` structs with public field shapes (or accessor methods) sufficient for an `optimize(plan) -> plan` rewrite pass to inspect and rebuild the tree.
- Future passes will be implemented as standalone functions in `hyphae/src/map_query/optimize/`.
- Candidate B-era passes:
  - **Index dedup**: detect joins keying on identical extractors against the same source; share one hash index.
  - **Projection pushdown**: track field reads at each stage; trim columns at the source.
  - **Join reordering**: cardinality-aware reordering of N-way join chains.

None of these are written in phase A, but the trait + struct shapes leave room for them without rework.

## Open questions resolved during brainstorm

- **Trait name**: `MapQuery<K, V>` (not `CellMapPlan`, not `Query`).
- **Consume vs borrow**: consume both `self` and RHS inputs, mirroring Pipeline's consuming `self` (post-flip).
- **Clone**: not implemented on plan nodes; users materialize first to get a `Clone`-able `CellMap`.
- **`subscribe`**: not on `MapQuery`; materialize first.
- **Method naming**: keep existing names (breakage at consumption, not at call site).
- **Index dedup / projection pushdown**: phase B, not phase A.

## Self-review notes

**Placeholder scan:** No `TBD` / `TODO` in this design.

**Internal consistency:** API surface table, materialize execution model, and architecture sections all describe the same flow.

**Scope:** This is one focused refactor analogous to the Pipeline refactor. No decomposition needed.

**Ambiguity:** "By value (consuming)" applied uniformly to `self` and RHS inputs across every operator method. No exceptions.
