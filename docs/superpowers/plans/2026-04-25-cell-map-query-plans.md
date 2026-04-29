# CellMap Query Plans Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a `MapQuery<K, V>` trait that turns chained `CellMap` operations (joins, projections, filters) into uncompiled query plans. Materializing the plan installs ONE subscription per root source and produces ONE output `CellMap` — vs today's per-stage `CellMap` allocations and cascading `ArcSwap` chains.

**Architecture:** Pure operators (`inner_join`, `left_join`, `left_semi_join`, `multi_left_join`, `project`, `project_many`, `project_cell`, `select`, `select_cell`, `count_by`, `group_by`) produce plan-node structs (`InnerJoinPlan`, `ProjectPlan`, etc.) instead of `CellMap<...>`. Plan nodes implement `MapQuery<K, V>`. Materialize walks the plan tree, installs one diff subscription per root source, and runs a fused diff-propagation execution that updates a single output `CellMap`. `CellMap` itself implements `MapQuery` (via blanket on `ReactiveMap`), so any reactive map composes as input to a plan operator.

**Tech Stack:**
- Rust 2024 edition
- Existing `hypha::CellMap`, `MapDiff`, `ReactiveMap`, `SubscriptionGuard`, `JoinState` machinery
- New: `MapQuery<K, V>` + `MapQueryInstall<K, V>` traits, `*Plan<...>` structs

---

## Vocabulary Lock-In

- **`MapQuery<K, V>`** — public trait. Uncompiled plan node. `Sized + Send + Sync + 'static`. Not `Clone`. One method: `materialize(self) -> CellMap<K, V, CellImmutable>`.
- **`MapQueryInstall<K, V>`** — crate-private trait. `materialize()` uses it to install diff subscribers.
- **`InnerJoinPlan<L, R, ...>`**, **`LeftJoinPlan<L, R, ...>`**, etc. — concrete plan structs, one per existing operator. Hold owned source(s) + closures.
- **`MapDiffSink<K, V>`** — `Arc<dyn Fn(&MapDiff<K, V>) + Send + Sync>`. Diff callback that downstream plan stages give upstream stages.
- **`.materialize(self)`** — consumes plan, returns `CellMap<K, V, CellImmutable>`.

---

## File Structure

**New files:**
- `hyphae/src/map_query/mod.rs` — `MapQuery`, `MapQueryInstall`, `materialize()` default impl, `MapDiffSink` type alias
- `hyphae/src/map_query/reactive_map_impl.rs` — blanket `impl<M: ReactiveMap> MapQuery<M::Key, M::Value> for M`
- `hyphae/src/tests/map_query_integration.rs` — integration tests
- `hyphae/benches/cell_map_chains.rs` — benchmark mirror to `pipeline_chains.rs`

**Modified files (in place — operator files do not move):**
- `hyphae/src/traits/collections/inner_join.rs` — add `InnerJoinPlan*` structs, change return types
- `hyphae/src/traits/collections/left_join.rs` — add `LeftJoinPlan*`
- `hyphae/src/traits/collections/left_semi_join.rs` — add `LeftSemiJoinPlan*`
- `hyphae/src/traits/collections/multi_left_join.rs` — add `MultiLeftJoinPlan*`
- `hyphae/src/traits/collections/project.rs` — add `ProjectPlan`
- `hyphae/src/traits/collections/project_many.rs` — add `ProjectManyPlan`
- `hyphae/src/traits/collections/project_cell.rs` — add `ProjectCellPlan`
- `hyphae/src/traits/collections/select.rs` — add `SelectPlan`
- `hyphae/src/traits/collections/select_cell.rs` — add `SelectCellPlan`
- `hyphae/src/traits/collections/count_by.rs` — add `CountByPlan`
- `hyphae/src/traits/collections/group_by.rs` — add `GroupByPlan`
- `hyphae/src/traits/collections/internal/join_runtime.rs` — refactor `run_join_runtime` into a `JoinExecutor` that takes a `MapDiffSink` instead of allocating its own `CellMap`. Keep the existing fn for now as a thin wrapper.
- `hyphae/src/traits/collections/internal/map_runtime.rs` — same refactor for the project/select runtime
- `hyphae/src/lib.rs` — add `pub mod map_query;` + re-export `MapQuery` and the plan structs
- `hyphae/src/traits/mod.rs` — re-export plan structs alongside their existing extension traits
- `hyphae/Cargo.toml` — register `cell_map_chains` bench

---

## Key Design Invariants

**Invariant 1: Plans don't subscribe.**
No `MapQuery` method takes a subscriber callback. The only way to observe output is `.materialize()` to a `CellMap`, then subscribe to that.

**Invariant 2: Plans aren't `Clone`.**
Cloning a plan would silently duplicate the join/projection work. Materialize first to get a `Clone`-able `CellMap`.

**Invariant 3: Plans consume their inputs.**
RHS of a join takes `R: MapQuery<...>` by value, not `&R`. Both `self` and the right input are consumed. Composition is direct: a plan node passed as input is moved into the receiving plan's struct.

**Invariant 4: Materialize allocates one output and one set of root subscriptions.**
A chain of N joins, materialized, produces ONE `CellMap<OK, OV, CellImmutable>`. Each unique root source gets ONE `subscribe_diffs_reactive` call. Per-stage `JoinState` lives in the closure captures owned by the materialized cell's guards.

**Invariant 5: `CellMap: MapQuery` via blanket on `ReactiveMap`.**
Any `ReactiveMap` is a `MapQuery` via blanket. `materialize()` on a plain `CellMap` returns a locked clone (no-op identity).

**Invariant 6: Method names unchanged.**
`inner_join`, `left_join_by`, `project`, `select`, etc. keep their names. Return type changes from `CellMap<...>` to `*Plan<...>`. Compile errors at consumption sites force `.materialize()` insertion.

---

## Execution Order

Tasks are strictly sequential. Each task ends with a commit; push only at the end.

Working directory: `/home/trevor/Code/hyphae/`. Branch: `feat/cellmap-query-plans` (create from `feat/pipelines` or whatever main branch this is being merged into). Use plain `cargo` with `--target-dir target/claude` for all cargo invocations. `flux` is NOT available locally.

---

## Task 1: Save baseline benchmarks

**Files:**
- Create: `hyphae/benches/cell_map_chains.rs`
- Modify: `hyphae/Cargo.toml` (register bench)

- [ ] **Step 1: Create `hyphae/benches/cell_map_chains.rs`**

```rust
//! Benchmarks targeting the CellMap query-plans migration.
//!
//! Run with `cargo bench --bench cell_map_chains -- --save-baseline pre-cellmap-query-plans`
//! BEFORE the refactor, then re-run with `--baseline pre-cellmap-query-plans`
//! after each port task to track effect.

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hyphae::{
    CellMap, MapDiff, ReactiveKeys,
    traits::{InnerJoinExt, LeftJoinExt, ProjectMapExt},
};

fn bench_join_chain_change_propagation(c: &mut Criterion) {
    let mut group = c.benchmark_group("join_chain_set");
    for chain_len in [1u32, 2, 3, 5].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(chain_len), chain_len, |b, &n| {
            // Build n source maps; chain n-1 left_joins by shared key.
            let sources: Vec<CellMap<String, i64>> = (0..n)
                .map(|_| {
                    let m = CellMap::<String, i64>::new();
                    for i in 0..16 {
                        m.insert(format!("k{}", i), i as i64);
                    }
                    m
                })
                .collect();

            // Today: chain via inner_join (allocates n-1 intermediate CellMaps).
            let mut last = sources[0].clone();
            for s in &sources[1..] {
                last = last.inner_join(s);
                // ⚠ This compiles pre-refactor (returns CellMap). After Task 2+ ports
                // inner_join to a plan, the chain construction needs `.materialize()`
                // between iterations OR a `seq_macro`-style fixed-depth approach (see
                // `pipeline_chains.rs`). Update this bench in Task 12 to match the
                // new API. For now: this captures the pre-refactor baseline.
            }

            let _last_held = last.clone();

            b.iter(|| {
                sources[0].insert(format!("k{}", black_box(0)), black_box(99));
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_join_chain_change_propagation);
criterion_main!(benches);
```

- [ ] **Step 2: Register bench in `hyphae/Cargo.toml`**

Add under existing `[[bench]]` entries:

```toml
[[bench]]
name = "cell_map_chains"
harness = false
```

- [ ] **Step 3: Compile bench**

Run: `cargo build --bench cell_map_chains --target-dir target/claude`
Expected: clean compile. Pre-refactor `inner_join` returns `CellMap` so chain reassignment typechecks.

- [ ] **Step 4: Save baseline**

Run: `cargo bench --bench cell_map_chains --target-dir target/claude -- --save-baseline pre-cellmap-query-plans`

Wait for it to complete. Record the printed numbers. They will be used in Task 12 for comparison.

- [ ] **Step 5: Commit**

```bash
git add hyphae/benches/cell_map_chains.rs hyphae/Cargo.toml
git commit -m "bench(hyphae): baseline cell-map join-chain propagation"
```

---

## Task 2: Define `MapQuery` and `MapQueryInstall` traits

**Files:**
- Create: `hyphae/src/map_query/mod.rs`
- Create: `hyphae/src/map_query/reactive_map_impl.rs`
- Modify: `hyphae/src/lib.rs`
- Create: `hyphae/src/tests/map_query_integration.rs`

- [ ] **Step 1: Write the failing integration test**

Create `hyphae/src/tests/map_query_integration.rs`:

```rust
//! Integration tests for MapQuery type.

use crate::{CellMap, MapQuery, ReactiveKeys};

#[test]
fn cell_map_is_map_query() {
    let m = CellMap::<String, i32>::new();
    m.insert("a".into(), 1);

    fn assert_query<K, V, Q: MapQuery<K, V>>(_: &Q)
    where
        K: std::hash::Hash + Eq + Clone + Send + Sync + 'static,
        V: Clone + Send + Sync + 'static,
    {
    }
    assert_query::<String, i32, _>(&m);
}

#[test]
fn materialize_plain_cell_map_is_identity() {
    let m = CellMap::<String, i32>::new();
    m.insert("a".into(), 10);

    let mat = m.clone().materialize();
    assert_eq!(mat.get_value(&"a".to_string()), Some(10));

    m.insert("b".into(), 20);
    assert_eq!(mat.get_value(&"b".to_string()), Some(20));
}
```

Add to `hyphae/src/tests/mod.rs`:

```rust
mod map_query_integration;
```

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo check -p hyphae --target-dir target/claude`
Expected: compile errors — `MapQuery` not found, `materialize` method missing.

- [ ] **Step 3: Create `hyphae/src/map_query/mod.rs`**

```rust
//! Uncompiled reactive map operation chains.
//!
//! A [`MapQuery`] is a recipe for a reactive map computation — a chain of
//! pure operators (joins, projections, selections) that has not yet been
//! materialized into a [`CellMap`]. Map queries deliberately do not implement
//! `subscribe`: to observe output you must call [`MapQuery::materialize`],
//! which installs ONE subscription per root source and returns a
//! subscribable cell map.

use std::sync::Arc;

use crate::{
    cell::CellImmutable,
    cell_map::{CellMap, CellMapInner, MapDiff},
    subscription::SubscriptionGuard,
    traits::CellValue,
};

pub(crate) mod reactive_map_impl;

/// Boxed diff sink shape used by every [`MapQueryInstall::install`] hop.
pub(crate) type MapDiffSink<K, V> = Arc<dyn Fn(&MapDiff<K, V>) + Send + Sync>;

/// Crate-private installer hook used by [`MapQuery::materialize`].
///
/// `install` subscribes upstream sources and pipes the output diff stream
/// to the provided sink. Returns guards that own the upstream subscriptions
/// and any per-stage state.
pub(crate) trait MapQueryInstall<K: CellValue + std::hash::Hash + Eq, V: CellValue>:
    Send + Sync + 'static
{
    fn install(&self, sink: MapDiffSink<K, V>) -> Vec<SubscriptionGuard>;
}

/// Uncompiled reactive map operation chain.
///
/// Built by chaining pure operators on a source (`CellMap` or another
/// `MapQuery`). Deliberately does not expose `subscribe` — call
/// [`MapQuery::materialize`] to produce a subscribable [`CellMap`].
///
/// # Not `Clone`
///
/// MapQueries are not `Clone`. Cloning would duplicate the join / projection
/// work; each clone's `materialize()` would install independent subscriptions
/// on the root sources and run separate executions. To share work across
/// consumers, materialize once into a `CellMap` (which IS `Clone` — Arc bump
/// referencing the same multicast cache) and clone that.
///
/// # Sealing
///
/// `MapQueryInstall` is `pub(crate)`, sealing `MapQuery` so external crates
/// cannot define new `MapQuery` shapes. New plan nodes are added inside this
/// crate.
#[allow(private_bounds)]
pub trait MapQuery<K, V>: MapQueryInstall<K, V> + Sized + Send + Sync + 'static
where
    K: CellValue + std::hash::Hash + Eq,
    V: CellValue,
{
    /// Compile the query into a [`CellMap`] and install root-source
    /// subscriptions.
    #[track_caller]
    fn materialize(self) -> CellMap<K, V, CellImmutable> {
        let output = CellMap::<K, V, crate::cell::CellMutable>::new();
        let weak = std::sync::Arc::downgrade(&output.inner);

        let sink: MapDiffSink<K, V> = Arc::new(move |diff| {
            let Some(inner) = weak.upgrade() else { return };
            let out = CellMap::<K, V, crate::cell::CellMutable> {
                inner,
                _marker: std::marker::PhantomData,
            };
            out.apply_diff_ref(diff);
        });

        let guards = self.install(sink);
        for g in guards {
            output.own(g);
        }
        output.lock()
    }
}
```

Note: `apply_diff_ref` is a new helper — it takes `&MapDiff<K, V>` and applies a single diff. If a similar method already exists with a different name, use it instead. If `apply_batch(Vec<MapDiff>)` is the only public form, change the sink signature to take `Vec<MapDiff>` (or wrap `[diff.clone()]`). Verify via `grep "fn apply" hyphae/src/cell_map.rs`.

- [ ] **Step 4: Verify or add `apply_diff_ref`**

Run: `grep -n "fn apply" hyphae/src/cell_map.rs`

If `apply_diff_ref(&self, diff: &MapDiff<K, V>)` doesn't exist, add it next to `apply_batch`:

```rust
/// Apply a single diff (clone-on-write) — used by MapQuery::materialize.
pub fn apply_diff_ref(&self, diff: &MapDiff<K, V>) {
    self.apply_batch(vec![diff.clone()]);
}
```

If `apply_batch` is `pub(crate)`, leave `apply_diff_ref` as `pub(crate)` too.

- [ ] **Step 5: Create `hyphae/src/map_query/reactive_map_impl.rs`**

```rust
//! Blanket `MapQuery` implementation for any [`ReactiveMap`].

use std::sync::Arc;

use super::{MapDiffSink, MapQuery, MapQueryInstall};
use crate::{
    subscription::SubscriptionGuard,
    traits::{CellValue, reactive_map::ReactiveMap},
};

impl<M> MapQueryInstall<M::Key, M::Value> for M
where
    M: ReactiveMap + Send + Sync + 'static,
    M::Key: CellValue + std::hash::Hash + Eq,
    M::Value: CellValue,
{
    fn install(&self, sink: MapDiffSink<M::Key, M::Value>) -> Vec<SubscriptionGuard> {
        let guard = self.subscribe_diffs_reactive(move |diff| sink(diff));
        vec![guard]
    }
}

impl<M> MapQuery<M::Key, M::Value> for M
where
    M: ReactiveMap + Send + Sync + 'static,
    M::Key: CellValue + std::hash::Hash + Eq,
    M::Value: CellValue,
{}
```

- [ ] **Step 6: Wire `lib.rs`**

Add `pub mod map_query;` after `pub mod cell_map;` (or wherever ordering keeps deps satisfied).

Add to the `pub use` block:

```rust
pub use map_query::MapQuery;
```

- [ ] **Step 7: Run tests**

Run: `cargo test -p hyphae --target-dir target/claude --lib map_query_integration 2>&1 | tail -10`

Expected: both new tests PASS.

- [ ] **Step 8: Run full check**

Run: `cargo check --workspace --target-dir target/claude`
Expected: clean.

- [ ] **Step 9: Commit**

```bash
git add hyphae/src/map_query/ hyphae/src/lib.rs hyphae/src/tests/map_query_integration.rs hyphae/src/tests/mod.rs hyphae/src/cell_map.rs
git commit -m "feat(hyphae): introduce MapQuery trait with materialize()

Adds MapQuery<K, V> as an uncompiled reactive map operation chain. Any
ReactiveMap implements MapQuery via blanket, so CellMap is both source
and identity case. materialize() installs one subscription per root
source and returns a CellMap<K, V, CellImmutable>.

No operators yet — next tasks port inner_join/left_join/project/etc.
onto MapQuery, replacing their CellMap-returning equivalents."
```

---

## Task 3: Refactor `join_runtime` to drive a sink

**Files:**
- Modify: `hyphae/src/traits/collections/internal/join_runtime.rs`

The existing `run_join_runtime` allocates its own `CellMap` and subscribes to two sources. Plan nodes need a version that drives a `MapDiffSink` so the materialized output can be a single shared cell map.

- [ ] **Step 1: Read current `run_join_runtime`**

Read `hyphae/src/traits/collections/internal/join_runtime.rs` end-to-end. Note that the inner state machine (`JoinState`, `apply_left_diff`, `apply_right_diff`, `recompute_impacted`) is generic and reusable.

- [ ] **Step 2: Add `install_join_runtime`**

Append to `join_runtime.rs` (after `run_join_runtime`):

```rust
/// Install join machinery that drives `sink` instead of allocating an output map.
///
/// Subscribes to both sources, maintains the join state, and pushes resulting
/// `MapDiff`s into the sink one at a time. Returns the two subscription guards
/// (caller owns them — typically attaches them to the materialized output).
pub(crate) fn install_join_runtime<L, R, JK, OK, OV, FL, FR, FO>(
    left: &L,
    right: &R,
    left_join_key: FL,
    right_join_key: FR,
    compute_rows: FO,
    sink: crate::map_query::MapDiffSink<OK, OV>,
) -> Vec<SubscriptionGuard>
where
    L: ReactiveMap,
    R: ReactiveMap,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    FL: Fn(&L::Key, &L::Value) -> JK + Send + Sync + 'static,
    FR: Fn(&R::Key, &R::Value) -> JK + Send + Sync + 'static,
    FO: Fn(&L::Key, &L::Value, &[(R::Key, R::Value)]) -> Vec<(OK, OV)> + Send + Sync + 'static,
{
    let state = Arc::new(Mutex::new(JoinState::<
        L::Key,
        L::Value,
        R::Key,
        R::Value,
        JK,
        OK,
        OV,
    >::default()));
    let left_join_key = Arc::new(left_join_key);
    let right_join_key = Arc::new(right_join_key);
    let compute_rows = Arc::new(compute_rows);

    let left_guard = left.subscribe_diffs_reactive({
        let state = state.clone();
        let left_join_key = left_join_key.clone();
        let compute_rows = compute_rows.clone();
        let sink = sink.clone();
        move |diff| {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            let mut impacted: HashSet<L::Key> = HashSet::new();
            apply_left_diff(&mut state, diff, left_join_key.as_ref(), &mut impacted);
            let changes = recompute_impacted(&mut state, impacted, compute_rows.as_ref());
            drop(state);
            for change in changes {
                sink(&change);
            }
        }
    });

    let right_guard = right.subscribe_diffs_reactive({
        let state = state.clone();
        let right_join_key = right_join_key.clone();
        let compute_rows = compute_rows.clone();
        let sink = sink.clone();
        move |diff| {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            let mut impacted: HashSet<L::Key> = HashSet::new();
            apply_right_diff(&mut state, diff, right_join_key.as_ref(), &mut impacted);
            let changes = recompute_impacted(&mut state, impacted, compute_rows.as_ref());
            drop(state);
            for change in changes {
                sink(&change);
            }
        }
    });

    vec![left_guard, right_guard]
}
```

You'll need to add `use crate::subscription::SubscriptionGuard;` at the top.

- [ ] **Step 3: Refactor `run_join_runtime` to call `install_join_runtime`**

Rewrite `run_join_runtime` body to delegate:

```rust
pub(crate) fn run_join_runtime<L, R, JK, OK, OV, FL, FR, FO>(
    left: &L,
    right: &R,
    _op_name: &str,
    left_join_key: FL,
    right_join_key: FR,
    compute_rows: FO,
) -> CellMap<OK, OV, CellImmutable>
where
    L: ReactiveMap,
    R: ReactiveMap,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    FL: Fn(&L::Key, &L::Value) -> JK + Send + Sync + 'static,
    FR: Fn(&R::Key, &R::Value) -> JK + Send + Sync + 'static,
    FO: Fn(&L::Key, &L::Value, &[(R::Key, R::Value)]) -> Vec<(OK, OV)> + Send + Sync + 'static,
{
    let output = CellMap::<OK, OV, CellMutable>::new();
    let weak = Arc::downgrade(&output.inner);

    let sink: crate::map_query::MapDiffSink<OK, OV> = Arc::new(move |diff| {
        let Some(inner) = weak.upgrade() else { return };
        let out = CellMap::<OK, OV, CellMutable> {
            inner,
            _marker: PhantomData,
        };
        out.apply_diff_ref(diff);
    });

    let guards = install_join_runtime(left, right, left_join_key, right_join_key, compute_rows, sink);
    for g in guards {
        output.own(g);
    }
    output.lock()
}
```

- [ ] **Step 4: Run all collection tests**

Run: `cargo test -p hyphae --target-dir target/claude --lib traits::collections 2>&1 | tail -10`
Expected: all existing collection tests still pass.

- [ ] **Step 5: Commit**

```bash
git add hyphae/src/traits/collections/internal/join_runtime.rs
git commit -m "refactor(hyphae): split join_runtime into sink-driven install + cell allocator

install_join_runtime drives an external MapDiffSink instead of allocating
its own CellMap. run_join_runtime is now a thin wrapper that allocates
the output and constructs a sink — preserving its existing public API
while exposing the sink-driven core for MapQuery plan nodes."
```

---

## Task 4: Refactor `map_runtime` to drive a sink

**Files:**
- Modify: `hyphae/src/traits/collections/internal/map_runtime.rs`

Same pattern as Task 3. Read `map_runtime.rs`, identify the part that subscribes + maintains state + pushes diffs to an output map. Split into `install_map_runtime` (sink-driven) and a thin `run_map_runtime` wrapper.

- [ ] **Step 1: Read `map_runtime.rs`**
- [ ] **Step 2: Add `install_map_runtime` taking a sink**
- [ ] **Step 3: Refactor `run_map_runtime` to delegate**
- [ ] **Step 4: Run tests**

Run: `cargo test -p hyphae --target-dir target/claude --lib 2>&1 | tail -5`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add hyphae/src/traits/collections/internal/map_runtime.rs
git commit -m "refactor(hyphae): split map_runtime into sink-driven install + cell allocator"
```

---

## Task 5: Port `inner_join` onto `MapQuery`

**Files:**
- Modify: `hyphae/src/traits/collections/inner_join.rs` (in place — do NOT move)
- Modify: `hyphae/src/traits/collections/mod.rs` (re-export new struct)
- Modify: `hyphae/src/traits/mod.rs`, `hyphae/src/lib.rs` (re-export)
- Test: `hyphae/src/tests/map_query_integration.rs`

- [ ] **Step 1: Write failing test**

Append to `map_query_integration.rs`:

```rust
use crate::traits::InnerJoinExt;

#[test]
fn inner_join_pipeline_does_not_allocate_intermediate() {
    let l = CellMap::<String, i32>::new();
    let r = CellMap::<String, i32>::new();
    l.insert("a".into(), 1);
    l.insert("b".into(), 2);
    r.insert("a".into(), 10);

    let plan = l.clone().inner_join(r.clone());
    // plan is an InnerJoinPlan, not a CellMap.
    let mat = plan.materialize();
    assert_eq!(mat.get_value(&"a".to_string()), Some((1, 10)));
    assert_eq!(mat.get_value(&"b".to_string()), None);

    r.insert("b".into(), 20);
    assert_eq!(mat.get_value(&"b".to_string()), Some((2, 20)));
}

#[test]
fn inner_join_chain_installs_one_subscription_per_root() {
    let a = CellMap::<String, i32>::new();
    let b = CellMap::<String, i32>::new();
    let c = CellMap::<String, i32>::new();
    a.insert("k".into(), 1);
    b.insert("k".into(), 2);
    c.insert("k".into(), 3);

    let initial_a = a.subscriber_count();
    let initial_b = b.subscriber_count();
    let initial_c = c.subscriber_count();

    let mat = a
        .clone()
        .inner_join(b.clone())
        .inner_join(c.clone())
        .materialize();

    assert_eq!(a.subscriber_count(), initial_a + 1, "a should have exactly one new sub");
    assert_eq!(b.subscriber_count(), initial_b + 1);
    assert_eq!(c.subscriber_count(), initial_c + 1);

    a.insert("k".into(), 99);
    let v = mat.get_value(&"k".to_string()).unwrap();
    assert_eq!(v, ((99, 2), 3));
}
```

If `CellMap::subscriber_count` doesn't exist, add a small accessor:

```rust
// in cell_map.rs
pub fn subscriber_count(&self) -> usize {
    self.inner.subscribers.len()
}
```

- [ ] **Step 2: Run, verify failure**

Run: `cargo check -p hyphae --target-dir target/claude`
Expected: errors — `inner_join` returns `CellMap`, missing `materialize`, `subscriber_count` (if added in Step 1) might need wiring.

- [ ] **Step 3: Read current `inner_join.rs`**

Note: three methods (`inner_join`, `inner_join_fk`, `inner_join_by`) all delegate to `run_join_runtime`.

- [ ] **Step 4: Add `InnerJoinByKeyPlan` and `InnerJoinByPairPlan` structs**

The two return shapes (by-key and by-pair) need separate plan structs because their output key types differ.

Append to `inner_join.rs`:

```rust
use std::marker::PhantomData;
use std::sync::Arc;

use crate::{
    map_query::{MapDiffSink, MapQuery, MapQueryInstall},
    subscription::SubscriptionGuard,
    traits::collections::internal::join_runtime::install_join_runtime,
};

/// Plan node for `left.inner_join(right)` (key-equal).
pub struct InnerJoinByKeyPlan<L, R>
where
    L: MapQuery<<L as MapQueryShape>::K, <L as MapQueryShape>::V>,
    R: MapQuery<<R as MapQueryShape>::K, <R as MapQueryShape>::V>,
{
    left: L,
    right: R,
}
```

Wait — the `L: MapQuery<K, V>` bound needs explicit K, V parameters. Plan nodes need to track their input/output key/value types. Use phantom-bounded structs:

```rust
pub struct InnerJoinByKeyPlan<L, LV, R, RV, K>
where
    L: MapQuery<K, LV>,
    R: MapQuery<K, RV>,
    K: CellValue + std::hash::Hash + Eq,
    LV: CellValue,
    RV: CellValue,
{
    left: L,
    right: R,
    _types: PhantomData<(LV, RV, K)>,
}

impl<L, LV, R, RV, K> MapQueryInstall<K, (LV, RV)>
    for InnerJoinByKeyPlan<L, LV, R, RV, K>
where
    L: MapQuery<K, LV>,
    R: MapQuery<K, RV>,
    K: CellValue + Hash + Eq,
    LV: CellValue,
    RV: CellValue,
{
    fn install(&self, sink: MapDiffSink<K, (LV, RV)>) -> Vec<SubscriptionGuard> {
        // The wrinkle: install_join_runtime currently takes &L, &R as
        // ReactiveMap. We need a shape that takes MapQuery sources and
        // composes their installs. Use a small adapter:
        //
        //   1. Materialize the left and right inputs into intermediate
        //      MutableCellMaps. (This re-allocates intermediates and
        //      defeats the win — DO NOT use this path for the final impl.)
        //
        // Correct path: refactor install_join_runtime to take MapQuery
        // sources via their MapQueryInstall::install method. The join state
        // gets fed from the sources' diff streams, not from
        // subscribe_diffs_reactive directly.
        //
        // See Task 5 Step 5 for the actual install_join_runtime_via_query.
        unimplemented!("implement via install_join_runtime_via_query — see step 5")
    }
}

#[allow(private_bounds)]
impl<L, LV, R, RV, K> MapQuery<K, (LV, RV)> for InnerJoinByKeyPlan<L, LV, R, RV, K>
where
    L: MapQuery<K, LV>,
    R: MapQuery<K, RV>,
    K: CellValue + Hash + Eq,
    LV: CellValue,
    RV: CellValue,
{}
```

- [ ] **Step 5: Add `install_join_runtime_via_query` in `join_runtime.rs`**

This is the key change. Modify `join_runtime.rs` to add a version that takes `MapQuery` inputs:

```rust
/// Like install_join_runtime but takes MapQuery inputs (not raw ReactiveMap).
/// The two source streams are obtained by calling install() on each.
pub(crate) fn install_join_runtime_via_query<LK, LV, RK, RV, JK, OK, OV, L, R, FL, FR, FO>(
    left: L,
    right: R,
    left_join_key: FL,
    right_join_key: FR,
    compute_rows: FO,
    sink: crate::map_query::MapDiffSink<OK, OV>,
) -> Vec<SubscriptionGuard>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    L: crate::map_query::MapQuery<LK, LV>,
    R: crate::map_query::MapQuery<RK, RV>,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> Vec<(OK, OV)> + Send + Sync + 'static,
{
    let state = Arc::new(Mutex::new(JoinState::<LK, LV, RK, RV, JK, OK, OV>::default()));
    let left_join_key = Arc::new(left_join_key);
    let right_join_key = Arc::new(right_join_key);
    let compute_rows = Arc::new(compute_rows);

    // Left stream sink: applies left diffs to state and emits output diffs.
    let left_sink: crate::map_query::MapDiffSink<LK, LV> = {
        let state = state.clone();
        let left_join_key = left_join_key.clone();
        let compute_rows = compute_rows.clone();
        let sink = sink.clone();
        Arc::new(move |diff| {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            let mut impacted: HashSet<LK> = HashSet::new();
            apply_left_diff(&mut state, diff, left_join_key.as_ref(), &mut impacted);
            let changes = recompute_impacted(&mut state, impacted, compute_rows.as_ref());
            drop(state);
            for change in changes {
                sink(&change);
            }
        })
    };

    // Right stream sink (analogous).
    let right_sink: crate::map_query::MapDiffSink<RK, RV> = {
        let state = state.clone();
        let right_join_key = right_join_key.clone();
        let compute_rows = compute_rows.clone();
        let sink = sink.clone();
        Arc::new(move |diff| {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            let mut impacted: HashSet<LK> = HashSet::new();
            apply_right_diff(&mut state, diff, right_join_key.as_ref(), &mut impacted);
            let changes = recompute_impacted(&mut state, impacted, compute_rows.as_ref());
            drop(state);
            for change in changes {
                sink(&change);
            }
        })
    };

    let mut guards = left.install(left_sink);
    guards.extend(right.install(right_sink));
    guards
}
```

This is the heart of the refactor. The key trick: the source's `install(sink)` recursively wires up upstream — so a chain `a.inner_join(b).inner_join(c).materialize()` produces a tree of installs, each contributing its own guards, all owned by the final cell map.

- [ ] **Step 6: Implement `InnerJoinByKeyPlan::install` using `install_join_runtime_via_query`**

```rust
impl<L, LV, R, RV, K> MapQueryInstall<K, (LV, RV)>
    for InnerJoinByKeyPlan<L, LV, R, RV, K>
where
    L: MapQuery<K, LV>,
    R: MapQuery<K, RV>,
    K: CellValue + Hash + Eq,
    LV: CellValue,
    RV: CellValue,
{
    fn install(self: &Self, sink: MapDiffSink<K, (LV, RV)>) -> Vec<SubscriptionGuard> {
        // self.left and self.right are owned values, but install takes &self.
        // We need to install on owned copies — but MapQuery isn't Clone.
        //
        // Solution: install takes self by value. Change the trait method
        // signature in map_query/mod.rs:
        //
        //     fn install(self, sink: MapDiffSink<K, V>) -> Vec<SubscriptionGuard>;
        //
        // (and update materialize accordingly: `self.install(sink)` works
        // because self is moved into materialize too.)
        unreachable!("see step 6.5")
    }
}
```

- [ ] **Step 6.5: Change `MapQueryInstall::install` to take `self` by value**

In `hyphae/src/map_query/mod.rs`:

```rust
pub(crate) trait MapQueryInstall<K, V>: Sized + Send + Sync + 'static
where
    K: CellValue + std::hash::Hash + Eq,
    V: CellValue,
{
    fn install(self, sink: MapDiffSink<K, V>) -> Vec<SubscriptionGuard>;
}
```

(Trait already has `Sized` because of `Self: Sized` for the `self` receiver.)

Update `MapQuery::materialize`:

```rust
fn materialize(self) -> CellMap<K, V, CellImmutable> {
    let output = ...;
    ...
    let guards = self.install(sink); // already self by value
    ...
}
```

The blanket `impl<M: ReactiveMap> MapQueryInstall` needs `M: ReactiveMap + Send + Sync + 'static` and `install(self, sink)` — but a `ReactiveMap` is typically referenced (e.g., `&CellMap`), so we install via `self.subscribe_diffs_reactive(...)` and the subscription guard owns the closure. `self` being moved into install is fine — subscribe_diffs_reactive only needs `&self` momentarily.

```rust
impl<M> MapQueryInstall<M::Key, M::Value> for M
where
    M: ReactiveMap + Send + Sync + 'static,
    ...
{
    fn install(self, sink: MapDiffSink<M::Key, M::Value>) -> Vec<SubscriptionGuard> {
        let guard = self.subscribe_diffs_reactive(move |diff| sink(diff));
        vec![guard]
    }
}
```

Wait — this drops `self` after `subscribe_diffs_reactive`. If `self` is a `CellMap` and the subscription requires the underlying inner to live, dropping `self` would also drop the last `Arc<CellMapInner>`. That's wrong.

Fix: the subscribe_diffs_reactive callback must keep the source alive. Move `self` into the callback:

```rust
fn install(self, sink: MapDiffSink<M::Key, M::Value>) -> Vec<SubscriptionGuard> {
    let source = self;
    let guard = source.subscribe_diffs_reactive(move |diff| sink(diff));
    // We need to retain `source` so the underlying CellMapInner doesn't drop.
    // BUT: subscribe_diffs_reactive registers the callback inside the inner,
    // so the inner outlives any external refs — actually, subscriber is held
    // by the inner via dashmap::entry. Verify in cell_map.rs whether the
    // subscription guard alone keeps the inner alive.
    //
    // If guard does NOT keep inner alive, attach source to a sink-style
    // owner: wrap source in the guard's Drop, or use the existing `own()`
    // pattern on a containing CellMap.
    vec![guard]
}
```

Read `cell_map.rs::SubscriptionGuard` and `subscribe_diffs_reactive` to verify. If the guard alone is insufficient, change install to additionally return the source-owning Arc.

- [ ] **Step 6.6: Verify `SubscriptionGuard` semantics**

```bash
grep -n "fn subscribe_diffs_reactive\|fn drop\|fn unsubscribe\|impl.*SubscriptionGuard" hyphae/src/cell_map.rs hyphae/src/subscription.rs
```

If subscription only keeps the callback alive (not the source), the source ref must be moved into the callback's closure. Update the install impl:

```rust
fn install(self, sink: MapDiffSink<M::Key, M::Value>) -> Vec<SubscriptionGuard> {
    // Hold `self` inside the callback so dropping the guard also drops
    // the source ref. (Cell-map source is cheap to Arc-keep.)
    let source = std::sync::Arc::new(self);
    let source_for_cb = source.clone();
    let guard = source.subscribe_diffs_reactive(move |diff| {
        let _keepalive = &source_for_cb;
        sink(diff);
    });
    vec![guard]
}
```

If this gets too clever, an alternative is for `MapQueryInstall::install` to return `(Vec<SubscriptionGuard>, Box<dyn Any + Send + Sync>)` where the second is an "owner bag". For now, keep it simple — capture in the closure.

- [ ] **Step 7: Now finish `InnerJoinByKeyPlan::install`**

```rust
impl<L, LV, R, RV, K> MapQueryInstall<K, (LV, RV)>
    for InnerJoinByKeyPlan<L, LV, R, RV, K>
where
    L: MapQuery<K, LV>,
    R: MapQuery<K, RV>,
    K: CellValue + Hash + Eq,
    LV: CellValue,
    RV: CellValue,
{
    fn install(self, sink: MapDiffSink<K, (LV, RV)>) -> Vec<SubscriptionGuard> {
        install_join_runtime_via_query::<K, LV, K, RV, K, K, (LV, RV), _, _, _, _, _>(
            self.left,
            self.right,
            |k, _| k.clone(),
            |k, _| k.clone(),
            |left_k, left_v, rights| {
                rights
                    .iter()
                    .map(|(_, rv)| (left_k.clone(), (left_v.clone(), rv.clone())))
                    .collect()
            },
            sink,
        )
    }
}
```

- [ ] **Step 8: Change `InnerJoinExt::inner_join` to return the plan**

Replace the trait/impl in `inner_join.rs`:

```rust
pub trait InnerJoinExt<K, V>: MapQuery<K, V>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    fn inner_join<R, RV>(
        self,
        right: R,
    ) -> InnerJoinByKeyPlan<Self, V, R, RV, K>
    where
        R: MapQuery<K, RV>,
        RV: CellValue,
    {
        InnerJoinByKeyPlan {
            left: self,
            right,
            _types: PhantomData,
        }
    }

    // ... inner_join_fk and inner_join_by similarly, returning InnerJoinByPairPlan
}

impl<K, V, M> InnerJoinExt<K, V> for M
where
    K: CellValue + Hash + Eq,
    V: CellValue,
    M: MapQuery<K, V>,
{}
```

Define `InnerJoinByPairPlan<L, LK, LV, R, RK, RV, JK, FL, FR>` similarly with a generic key extractor pair, and wire `inner_join_fk`/`inner_join_by` to construct it.

- [ ] **Step 9: Wire re-exports**

In `hyphae/src/traits/collections/mod.rs`, change the existing line for inner_join's re-export to also export the plan structs:

```rust
pub use inner_join::{InnerJoinExt, InnerJoinByKeyPlan, InnerJoinByPairPlan};
```

In `hyphae/src/traits/mod.rs`, adjust the long `pub use collections::*` line if it lists the operator trait by name (add the plan structs).

In `hyphae/src/lib.rs`, ensure `InnerJoinByKeyPlan` and `InnerJoinByPairPlan` are re-exported alongside `InnerJoinExt`.

- [ ] **Step 10: Run tests**

```bash
cargo test -p hyphae --target-dir target/claude --lib map_query_integration 2>&1 | tail -10
cargo test -p hyphae --target-dir target/claude --lib 2>&1 | tail -5
```

Expected: new tests pass; all 256+ existing tests pass.

- [ ] **Step 11: Fix internal callers**

Run: `cargo check --workspace --target-dir target/claude 2>&1 | grep -A1 "inner_join" | head -20`

Internal callers using `inner_join` and consuming as `CellMap` need `.materialize()`. Insert as needed. Common shape: `a.inner_join(b).materialize()` where today it's just `a.inner_join(&b)`.

The signature change `&R` → `R` also requires source-ownership at call sites. Use `right.clone()` if right is reused. Audit and fix each broken site.

- [ ] **Step 12: Run full check**

```bash
cargo check --workspace --target-dir target/claude
cargo test -p hyphae --target-dir target/claude --lib 2>&1 | tail -5
```

- [ ] **Step 13: Commit**

```bash
git add hyphae/src/traits/collections/inner_join.rs hyphae/src/traits/collections/mod.rs hyphae/src/traits/collections/internal/join_runtime.rs hyphae/src/traits/mod.rs hyphae/src/lib.rs hyphae/src/tests/map_query_integration.rs hyphae/src/cell_map.rs
git commit -m "feat(hyphae): port inner_join onto MapQuery

inner_join, inner_join_fk, inner_join_by now return InnerJoin*Plan
nodes instead of CellMap<...>. Composable: any MapQuery is a valid
input. Materialize collapses chained joins into one output cell map
with one subscription per root source."
```

---

## Task 6: Port `left_join` onto `MapQuery`

**Files:**
- Modify: `hyphae/src/traits/collections/left_join.rs`
- Re-exports + tests as in Task 5

Same pattern as Task 5. Methods: `left_join`, `left_join_by`. Plan structs: `LeftJoinByKeyPlan`, `LeftJoinByPairPlan`. The output type for `left_join` is `(LV, Vec<RV>)` per the existing impl — preserve that.

Steps 1–13 mirror Task 5. Failing tests in `map_query_integration.rs`:

```rust
use crate::traits::LeftJoinExt;

#[test]
fn left_join_pipeline_includes_unmatched_left() {
    let l = CellMap::<String, i32>::new();
    let r = CellMap::<String, i32>::new();
    l.insert("a".into(), 1);
    l.insert("b".into(), 2);
    r.insert("a".into(), 10);
    r.insert("a".into(), 11); // Wait — this is upsert; only 11 remains.
    // Adjust: we want a key in left absent from right.

    let mat = l.clone().left_join(r.clone()).materialize();
    assert_eq!(mat.get_value(&"a".to_string()), Some((1, vec![11])));
    assert_eq!(mat.get_value(&"b".to_string()), Some((2, vec![])));
}
```

Commit:

```
feat(hyphae): port left_join onto MapQuery
```

---

## Task 7: Port `left_semi_join` onto `MapQuery`

**Files:**
- Modify: `hyphae/src/traits/collections/left_semi_join.rs`
- Re-exports + tests

Same shape. Methods: `left_semi_join`, `left_semi_join_by`. Plan struct: `LeftSemiJoinByKeyPlan`, `LeftSemiJoinByPairPlan`. Output type matches the existing semantics (left value, no right tuple).

Failing test:

```rust
#[test]
fn left_semi_join_keeps_left_with_match() {
    let l = CellMap::<String, i32>::new();
    let r = CellMap::<String, i32>::new();
    l.insert("a".into(), 1);
    l.insert("b".into(), 2);
    r.insert("a".into(), 10);

    let mat = l.clone().left_semi_join(r.clone()).materialize();
    assert_eq!(mat.get_value(&"a".to_string()), Some(1));
    assert_eq!(mat.get_value(&"b".to_string()), None);
}
```

Commit:

```
feat(hyphae): port left_semi_join onto MapQuery
```

---

## Task 8: Port `multi_left_join` onto `MapQuery`

**Files:**
- Modify: `hyphae/src/traits/collections/multi_left_join.rs`

Multi-input. The plan node holds a `Vec<Box<dyn AnyMapQuery>>` (or a tuple) of right inputs. The install hook installs each right input with its own sink-into-shared-state.

Investigate the existing `multi_join_runtime`. Refactor it analogously to Task 3 (sink-driven `install_multi_join_runtime`).

Plan struct: `MultiLeftJoinPlan<L, ...>`. May need additional bounds; consult the existing trait shape.

Failing test mirrors existing multi_left_join unit tests; adapt with `.materialize()`.

Commit: `feat(hyphae): port multi_left_join onto MapQuery`.

---

## Task 9: Port `project` and `project_many` onto `MapQuery`

**Files:**
- Modify: `hyphae/src/traits/collections/project.rs`
- Modify: `hyphae/src/traits/collections/project_many.rs`
- Re-exports + tests

`ProjectPlan<S, ...>` and `ProjectManyPlan<S, ...>` over a single `MapQuery` source. Use `install_map_runtime` (Task 4) — give it a sink that filters/expands per the project closure.

Failing test:

```rust
use crate::traits::ProjectMapExt;

#[test]
fn project_pipeline() {
    let src = CellMap::<String, i32>::new();
    src.insert("a".into(), 5);

    let mat = src.clone()
        .project(|k, v| Some((format!("p:{k}"), v * 10)))
        .materialize();
    assert_eq!(mat.get_value(&"p:a".to_string()), Some(50));

    src.insert("b".into(), 7);
    assert_eq!(mat.get_value(&"p:b".to_string()), Some(70));
}
```

Commit: `feat(hyphae): port project and project_many onto MapQuery`.

---

## Task 10: Port `project_cell` onto `MapQuery`

**Files:**
- Modify: `hyphae/src/traits/collections/project_cell.rs`

`ProjectCellPlan<S, ...>` — per-row mapper returns a `Watchable<Option<(K2, V2)>>` (currently). This is more complex because each row's mapper output is itself reactive. The mapper now likely produces a `Pipeline<Option<(K2, V2)>>` (per the Pipeline refactor). Adapt the install: when the source row appears, install the per-row pipeline as a child subscription; when the row's value changes, the pipeline's emission propagates to the output sink.

Document the per-row install lifecycle clearly. This is the most intricate operator port — give it focused attention.

Commit: `feat(hyphae): port project_cell onto MapQuery`.

---

## Task 11: Port `select`, `select_cell`, `count_by`, `group_by` onto `MapQuery`

**Files:**
- Modify each of the four files in `hyphae/src/traits/collections/`

Each follows the same shape: a plan struct, an `install` that drives the sink via the existing or sink-refactored runtime.

`select` and `select_cell` are filters — they emit a subset of source rows. `count_by` and `group_by` are aggregates — they maintain group state and emit grouped rows.

Test each with at least one `_pipeline` integration test in `map_query_integration.rs`.

Commit one task per operator, OR a single squash commit `feat(hyphae): port select/select_cell/count_by/group_by onto MapQuery` if the changes are small.

---

## Task 12: Adapt `cell_map_chains.rs` bench to new API and run comparison

**Files:**
- Modify: `hyphae/benches/cell_map_chains.rs`

Pre-refactor, the bench used `last = last.inner_join(s)` in a loop. Post-refactor, types nest. Use the `seq_macro::seq!` pattern from `pipeline_chains.rs`:

```rust
use seq_macro::seq;

macro_rules! bench_chain {
    ($group:expr, $depth:literal, $tail:literal) => { ... };
}

fn bench_join_chain_change_propagation(c: &mut Criterion) {
    let mut group = c.benchmark_group("join_chain_set");
    bench_chain!(group, 1, 0);
    bench_chain!(group, 2, 1);
    bench_chain!(group, 3, 2);
    bench_chain!(group, 5, 4);
    group.finish();
}
```

Inside each bench: build the chain via macro, materialize once, install a subscriber that counts emissions, then `b.iter` does a single source insert.

Run: `cargo bench --bench cell_map_chains --target-dir target/claude -- --baseline pre-cellmap-query-plans`

Capture the deltas. Expected: chain depth N propagation now flat (one ArcSwap regardless of N).

Write results to `docs/superpowers/plans/2026-04-25-cell-map-query-plans-bench-results.md`. Commit:

```
bench(hyphae): cell-map chain propagation comparison vs pre-cellmap baseline

(table of deltas)
```

---

## Task 13: Audit rship blast radius

**Files:**
- Read-only: `/home/trevor/Code/rship/`
- Create: `docs/superpowers/plans/2026-04-25-cell-map-query-plans-rship-audit.md`

Mirror the Pipeline rship audit. Grep counts:

```bash
cd /home/trevor/Code/rship
grep -rn "\.inner_join_by\|\.left_join_by\|\.left_semi_join_by\|\.multi_left_join_by" libs/ apps/ --include="*.rs" | wc -l
grep -rn "\.inner_join(\|\.left_join(\|\.left_semi_join(" libs/ apps/ --include="*.rs" | wc -l
grep -rn "\.project_cell(\|\.select_cell(\|\.project(\|\.project_many(\|\.select(\|\.count_by(\|\.group_by(" libs/ apps/ --include="*.rs" | wc -l
```

Sample 5–10 sites; verify `.materialize()` insertion is mechanical. Also note `&map` → `map.clone()` substitutions.

Write the audit. Commit:

```
docs(hyphae): audit rship blast radius for CellMap query plans
```

---

## Task 14: Update hyphae public docs

**Files:**
- Modify: `hyphae/src/lib.rs` (top-level doc)
- Modify: `hyphae/README.md`

Add a "Map Queries vs CellMap" subsection mirroring the "Pipelines vs Cells" one. Update Quick Start with a small `inner_join().project().materialize()` example.

Commit: `docs(hyphae): update Quick Start and README for MapQuery API`.

---

## Spec Coverage Check

Maps every spec section to a task:

- [x] `MapQuery<K, V>` trait without `subscribe` — Task 2
- [x] `MapQueryInstall<K, V>` crate-private installer — Task 2
- [x] `materialize(self) -> CellMap<K, V, CellImmutable>` — Task 2
- [x] `M: ReactiveMap → MapQuery` blanket — Task 2
- [x] Plan composability (plan as input to plan) — Task 5 (and inherent in trait design)
- [x] `inner_join`, `inner_join_fk`, `inner_join_by` ported — Task 5
- [x] `left_join`, `left_join_by` ported — Task 6
- [x] `left_semi_join`, `left_semi_join_by` ported — Task 7
- [x] `multi_left_join` ported — Task 8
- [x] `project`, `project_many` ported — Task 9
- [x] `project_cell` ported — Task 10
- [x] `select`, `select_cell`, `count_by`, `group_by` ported — Task 11
- [x] Internal callers fixed — folded into each port task (Step 11 of Task 5, etc.)
- [x] Bench comparing pre/post — Tasks 1 + 12
- [x] rship audit — Task 13
- [x] Public docs update — Task 14

## Out of Scope (explicitly deferred)

Per spec phase B:
- Index dedup across stages
- Projection pushdown
- Join reordering / cardinality-aware plans
- `optimize(plan) -> plan` rewrite passes

Plan-tree shape leaves room for these without rework.

## Self-Review Notes

**Placeholder scan:** No `TODO`, `TBD`, or "appropriate error handling" inside Task steps. Per-task references to "implement steps" point to subsequent steps within the same task.

**Type consistency:**
- `MapQuery<K, V>` consistent throughout.
- `MapQueryInstall<K, V>` consistent throughout; crate-private.
- Plan struct names (`InnerJoinByKeyPlan`, `LeftJoinByPairPlan`, `ProjectPlan`, etc.) consistent.
- `materialize` — one spelling.
- `MapDiffSink<K, V>` — single type alias.

**Spec coverage:** All sections mapped.

**Known friction points:**
- The `MapQueryInstall::install` consuming `self` interacts with the blanket `impl<M: ReactiveMap>`: dropping `M` after `subscribe_diffs_reactive` would drop the underlying inner cell map. Solution detailed in Task 5 Step 6.6 (capture source in callback). Validate during implementation; if cumbersome, consider returning `(Vec<SubscriptionGuard>, Box<dyn Any + Send + Sync>)` from install for an explicit owner bag.
- Each operator has multiple variants (e.g., inner_join, inner_join_fk, inner_join_by). Plan struct count grows ~3× number of operator traits. Consider a `Box<dyn ...>` simplification only if the explicit-typed approach gets unwieldy — but Pipeline showed explicit types compile cleanly even for deep chains with `seq_macro` test depth.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-04-25-cell-map-query-plans.md`.

Two execution options:

1. **Subagent-Driven (recommended)** — dispatch a fresh subagent per task, review between tasks, fast iteration.
2. **Inline Execution** — execute tasks in this session using executing-plans, batch execution with checkpoints.

If resuming later, the entry point is Task 1 (save baseline benches). Tasks 2–11 are sequential; Task 12 depends on Tasks 5–11 being done.
