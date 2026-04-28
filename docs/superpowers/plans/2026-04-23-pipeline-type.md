# Pipeline Type Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a `Pipeline<T>` type that represents an uncompiled reactive operation chain (map/filter/etc.) with no intermediate `Cell` allocations and no `subscribe` method — forcing a deliberate `.materialize()` boundary before consumers can subscribe, so memoization cost is always explicit at call sites.

**Architecture:** Pure operators (`map`, `filter`, `try_map`, `tap`, `map_ok`, `map_err`, `catch_error`, `unwrap_or`, `cold`, `finalize`) produce `Pipeline<T>` instead of `Cell<T, CellImmutable>`. `Pipeline` exposes `get()` (recomputes from source) and operator combinators, but no `subscribe`. Consumers call `.materialize()` to compile the chain into a `Cell<T, CellImmutable>` that installs a single subscription on the root source running the fused closure. Stateful operators (`scan`, `debounce`, `throttle`, `buffer_*`, `pairwise`, `state_transition`, `window`, joins, `with_latest_from`, `zip`, `merge`, `distinct`, `sample`, `delay`, `take`, `first`, `last`) keep their current signatures and return `Cell<T, CellImmutable>` directly — materialization is unavoidable for them because they hold state or synchronize multiple inputs. `Cell<T, M>` implements `Pipeline<T>`, so existing `cell.map(...)` call sites compile-break and force an explicit `.materialize()` insertion — the compile errors ARE the feature.

**Tech Stack:**
- Rust (workspace uses Rust 2024 edition based on existing code)
- `arc_swap::ArcSwap` for lock-free value storage
- `dashmap::DashMap` for subscriber tables
- `uuid::Uuid` for cell IDs
- Existing `hypha::Signal<T>`, `hypha::SubscriptionGuard`, `hypha::traits::{Watchable, Gettable, DepNode, CellValue}`

---

## Vocabulary Lock-In

Read this before implementing. These names are the API contract:

- **`Pipeline<T>`** — trait. Uncompiled op chain. Has `get()` + operator combinators + `materialize()`. No `subscribe`.
- **`PipelineInstall<T>`** — crate-private trait. Hook `materialize()` uses to install the fused closure onto the root source.
- **`MapPipeline<S, T, U, F>`**, **`FilterPipeline<S, T, P>`**, etc. — concrete pipeline structs produced by pure operators. One per operator.
- **`Cell<T, M>`** — unchanged type. Storage + multicast cache. Implements `Pipeline<T>` (so `cell.map(...)` returns a pipeline).
- **`.materialize(self)`** — consumes a pipeline and returns `Cell<T, CellImmutable>`. Installs one subscription on the root source.

Any naming drift from this list is a bug — fix it inline before moving on.

---

## File Structure

**New files:**
- `hyphae/src/pipeline/mod.rs` — `Pipeline` and `PipelineInstall` traits, `materialize()` default impl, re-exports
- `hyphae/src/pipeline/map.rs` — `MapPipeline` struct + `MapExt` on `Pipeline`
- `hyphae/src/pipeline/filter.rs` — `FilterPipeline` struct + `FilterExt` on `Pipeline`
- `hyphae/src/pipeline/try_map.rs` — `TryMapPipeline` + `TryMapExt` on `Pipeline`
- `hyphae/src/pipeline/tap.rs` — `TapPipeline` + `TapExt` on `Pipeline`
- `hyphae/src/pipeline/result_ext.rs` — `MapOkPipeline`, `MapErrPipeline`, `CatchErrorPipeline`, `UnwrapOrPipeline` + their exts
- `hyphae/src/pipeline/cell_impl.rs` — blanket `impl<T, M> Pipeline<T> for Cell<T, M>` + `impl PipelineInstall<T> for Cell<T, M>`
- `hyphae/src/tests/pipeline_integration.rs` — integration tests verifying fusion, materialize semantics, breaking-change markers

**Modified files:**
- `hyphae/src/lib.rs` — add `pub mod pipeline;` and re-export `Pipeline` + concrete pipeline structs
- `hyphae/src/traits/operators/mod.rs` — remove `mod map`, `mod filter`, `mod try_map`, `mod tap`, `mod result_ext`, `mod cold`, `mod finalize` + the `pub use` lines for them (these move under `pipeline/`)
- `hyphae/src/traits/operators/map.rs` — **DELETE** (moved to `pipeline/map.rs`)
- `hyphae/src/traits/operators/filter.rs` — **DELETE**
- `hyphae/src/traits/operators/try_map.rs` — **DELETE**
- `hyphae/src/traits/operators/tap.rs` — **DELETE**
- `hyphae/src/traits/operators/result_ext.rs` — **DELETE**
- `hyphae/src/traits/operators/cold.rs` — **REVIEW**: if purely transformational, move; otherwise leave
- `hyphae/src/traits/operators/finalize.rs` — **REVIEW**: probably side-effect on complete, may need special handling
- `hyphae/src/traits/mod.rs` — drop `MapExt`, `FilterExt`, `TryMapExt`, `TapExt`, result exts from the `pub use` in operators re-export (they now come from `pipeline::`)

**Unchanged files (but verify compile):**
- All stateful operator files under `hyphae/src/traits/operators/` (scan, debounce, etc.) keep their `Watchable<T>` bounds — they accept `Cell` OR `Pipeline::materialize()` output.

---

## Key Design Invariants

**Invariant 1: Pipelines do not subscribe. Period.**
No `Pipeline` method takes a subscriber callback. The only way to observe pipeline output is `.materialize()` to a `Cell`, then subscribe to the cell.

**Invariant 2: Pipelines are single-use by default.**
`materialize(self)` consumes. To materialize twice, clone the pipeline first (pipelines must be `Clone` when their source + op are `Clone`). Each materialize installs a fresh subscription on the root source.

**Invariant 3: `get()` on a pipeline recomputes.**
`pipeline.get()` walks `source.get()` and applies the op. Documented as "not for hot loops — materialize first."

**Invariant 4: Root-source subscription only.**
`materialize()` on a chained pipeline installs ONE subscription on the root cell, running the composed closure. No intermediate cells are created anywhere in the chain.

**Invariant 5: Stateful operators remain `Watchable<T>`-bounded.**
They take `&self` on a `Watchable<T>` and return `Cell<U, CellImmutable>`. Since `Cell` is `Watchable`, and `Pipeline::materialize()` returns a `Cell`, callers who want `pipeline.scan(...)` write `pipeline.materialize().scan(...)`. This is intentional — scan needs a subscription point, which means a cell must exist anyway.

**Invariant 6: `Cell: Pipeline<T>` — the breaking change.**
Today: `MapExt` is blanket-impl'd on `Watchable`. After: `MapExt` is blanket-impl'd on `Pipeline`, and `Cell: Pipeline`. So `cell.map(f)` now returns `MapPipeline`, not `Cell<U>`. Every `cell.map(f).subscribe(cb)` site breaks with a clear compiler error. Insert `.materialize()` between them.

---

## Execution Order

Tasks are strictly sequential — each depends on the previous. Do not parallelize. Each task ends with a commit; push only at the end.

Work in the hyphae repo at `/home/trevor/Code/hyphae/`. Use `cargo flux run check` (NOT raw `cargo check`) and `--target-dir target/claude` for any raw cargo invocations. Check `.bacon-locations` for pre-existing clippy errors before treating new warnings as your fault.

---

## Task 1: Define the `Pipeline` and `PipelineInstall` traits

**Files:**
- Create: `hyphae/src/pipeline/mod.rs`
- Create: `hyphae/src/pipeline/cell_impl.rs`
- Modify: `hyphae/src/lib.rs` (add `pub mod pipeline;` + re-exports)

- [ ] **Step 1: Write the failing integration test**

Create `hyphae/src/tests/pipeline_integration.rs`:

```rust
//! Integration tests for Pipeline type.

use crate::{Cell, Gettable, Mutable, Pipeline};

#[test]
fn cell_is_pipeline() {
    let c = Cell::new(10);
    // Cell: Pipeline<i32> — get() comes from Pipeline
    let v: i32 = Pipeline::get(&c);
    assert_eq!(v, 10);
}

#[test]
fn pipeline_materialize_roundtrip() {
    let c = Cell::new(5).with_name("src");
    // Pipeline::materialize on a plain Cell returns a Cell with the same value
    let mat = c.clone().materialize();
    assert_eq!(mat.get(), 5);

    c.set(99);
    assert_eq!(mat.get(), 99);
}
```

Add to `hyphae/src/tests/mod.rs`:

```rust
mod pipeline_integration;
```

(Check that file exists first. If `hyphae/src/tests/mod.rs` doesn't exist, create it with `mod pipeline_integration;` as the only line.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo flux run check -p hyphae`
Expected: compile errors — `Pipeline` not found, `materialize` method missing.

- [ ] **Step 3: Create `hyphae/src/pipeline/mod.rs`**

```rust
//! Uncompiled reactive operation chains.
//!
//! A [`Pipeline`] is a recipe for a reactive computation — a chain of pure
//! operators (`map`, `filter`, ...) that has not yet been materialized into
//! a [`Cell`]. Pipelines deliberately do not implement `subscribe`: to observe
//! output you must call [`Pipeline::materialize`], which installs a single
//! subscription on the root source and returns a subscribable cell.
//!
//! This design makes the memoization boundary explicit. Today, chaining
//! operators on a `Cell` creates an intermediate cell per operator — each
//! caching its value and notifying downstream subscribers. That is the right
//! choice when the derived value is consumed by many subscribers, but wasteful
//! when it is consumed by one. By moving pure operators onto `Pipeline`, the
//! cost of an intermediate cell is paid only when the caller explicitly asks
//! for one with `.materialize()`.
//!
//! # Example
//! ```
//! use hyphae::{Cell, Gettable, Mutable, Pipeline, MapExt};
//!
//! let src = Cell::new(10);
//! // map() returns a MapPipeline, not a Cell — no intermediate allocation
//! let pipeline = src.clone().map(|x| x * 2);
//! // materialize() installs one subscription on `src` running the fused closure
//! let doubled = pipeline.materialize();
//! assert_eq!(doubled.get(), 20);
//! src.set(50);
//! assert_eq!(doubled.get(), 100);
//! ```

use std::sync::Arc;

use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, DepNode, Gettable},
};

pub(crate) mod cell_impl;
pub mod map;
pub mod filter;
pub mod try_map;
pub mod tap;
pub mod result_ext;

pub use map::{MapPipeline, MapExt};
pub use filter::{FilterPipeline, FilterExt};
pub use try_map::{TryMapPipeline, TryMapExt};
pub use tap::{TapPipeline, TapExt};
pub use result_ext::{
    MapOkPipeline, MapOkExt,
    MapErrPipeline, MapErrExt,
    CatchErrorPipeline, CatchErrorExt,
    UnwrapOrPipeline, UnwrapOrExt,
};

/// Crate-private installer hook used by [`Pipeline::materialize`].
///
/// `install` subscribes the pipeline's composed callback to the root source
/// and returns the guard. The fused closure transforms root-source signals
/// into the pipeline's output signal type and invokes the provided callback.
///
/// This is separate from `Pipeline` so that the public trait stays minimal
/// and cannot be accidentally used to subscribe without materializing.
pub(crate) trait PipelineInstall<T: CellValue>: Send + Sync + 'static {
    /// Install `callback` such that every future value signal from the root
    /// source flows through the pipeline's composed op and invokes `callback`
    /// with the resulting `Signal<T>`.
    ///
    /// The returned guard owns the subscription on the root source. Dropping
    /// it ends the chain. The root-source `DepNode` is accessible via
    /// `guard.source()` for introspection.
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard;
}

/// Uncompiled reactive operation chain.
///
/// Pipelines are built by chaining pure operators on a source (`Cell` or
/// another `Pipeline`). They deliberately do not expose `subscribe` — call
/// [`Pipeline::materialize`] to produce a subscribable [`Cell`].
///
/// # Invariants
///
/// - `get()` recomputes from the root source on every call. Do not use in
///   hot loops — materialize first.
/// - `materialize(self)` consumes the pipeline and installs a single
///   subscription on the root source running the fully fused closure.
/// - No intermediate `Cell` is allocated anywhere in a pipeline chain.
pub trait Pipeline<T: CellValue>:
    Gettable<T> + PipelineInstall<T> + Clone + Send + Sync + 'static
{
    /// Compile the pipeline into a [`Cell`] and install a single subscription
    /// on the root source running the fused closure.
    ///
    /// This is the only way to observe pipeline output. Every subscribe in
    /// the codebase is on a cell, never on a pipeline — which is the point.
    #[track_caller]
    fn materialize(self) -> Cell<T, CellImmutable> {
        let initial = self.get();
        let cell = Cell::<T, CellMutable>::new(initial);
        let weak = cell.downgrade();

        // Skip the first re-emit: the pipeline's install() will synchronously
        // invoke `callback` with the current source value on subscription, and
        // we already captured it as the initial value. Without this guard the
        // materialized cell double-notifies with the same value on creation.
        let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let callback: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal: &Signal<T>| {
            if let Some(c) = weak.upgrade() {
                if matches!(signal, Signal::Value(_))
                    && first.swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    return;
                }
                c.notify(signal.clone());
            }
        });

        let guard = self.install(callback);
        cell.own(guard);
        cell.lock()
    }
}
```

- [ ] **Step 4: Create `hyphae/src/pipeline/cell_impl.rs`**

```rust
//! Blanket `Pipeline` implementation for `Cell`.

use std::sync::Arc;

use crate::{
    cell::Cell,
    pipeline::{Pipeline, PipelineInstall},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Watchable},
};

impl<T: CellValue, M: Send + Sync + 'static> PipelineInstall<T> for Cell<T, M> {
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
        // Delegate to the cell's native subscribe — the callback receives signals
        // untransformed because a plain Cell's pipeline op is identity.
        self.subscribe(move |signal| callback(signal))
    }
}

impl<T: CellValue, M: Send + Sync + 'static> Pipeline<T> for Cell<T, M> {}
```

- [ ] **Step 5: Update `hyphae/src/lib.rs`**

Add `pub mod pipeline;` after `pub mod subscription;` (around line 71). Add these re-exports near the other `pub use` block:

```rust
pub use pipeline::{
    Pipeline,
    MapPipeline, MapExt,
    FilterPipeline, FilterExt,
    TryMapPipeline, TryMapExt,
    TapPipeline, TapExt,
    MapOkPipeline, MapOkExt,
    MapErrPipeline, MapErrExt,
    CatchErrorPipeline, CatchErrorExt,
    UnwrapOrPipeline, UnwrapOrExt,
};
```

- [ ] **Step 6: Create empty stub files so `mod` declarations compile**

Create these files with only one line (`//! TODO: implement in next task`) each. They will be replaced in later tasks — this step only unblocks compilation.

```
hyphae/src/pipeline/map.rs
hyphae/src/pipeline/filter.rs
hyphae/src/pipeline/try_map.rs
hyphae/src/pipeline/tap.rs
hyphae/src/pipeline/result_ext.rs
```

Put placeholder in each:

```rust
//! TODO: implement in Task N.

pub struct Placeholder;
```

For each of `map.rs`, `filter.rs`, etc., the `Pipeline::mod.rs` re-exports names like `MapPipeline`, `MapExt` — to keep the mod compile-clean at this intermediate step, **replace the re-exports block in `pipeline/mod.rs` with a commented-out block for now**, and put this single line at the bottom instead:

```rust
// Re-exports are added as each concrete pipeline module is implemented.
```

Remove the re-exports from `lib.rs` for this intermediate step too — keep only `pub use pipeline::Pipeline;` for now.

- [ ] **Step 7: Run the integration test**

Run: `cargo flux run test -p hyphae -- pipeline_integration`
Expected: both tests PASS. `Pipeline::get(&c)` dispatches through the blanket `Cell: Pipeline` + `Cell: Gettable`. `materialize()` on the cell installs a subscribe via `PipelineInstall::install`, returns a new cell tracking the source.

- [ ] **Step 8: Run full check**

Run: `cargo flux run check` (from `/home/trevor/Code/hyphae/`)
Expected: clean, no warnings. Pre-existing errors in `.bacon-locations` can be ignored if unrelated.

- [ ] **Step 9: Commit**

```bash
git add hyphae/src/pipeline/ hyphae/src/lib.rs hyphae/src/tests/
git commit -m "feat(hyphae): introduce Pipeline trait with materialize()

Adds Pipeline<T> as an uncompiled reactive operation chain. Cell<T, M>
implements Pipeline, so cells participate as both source and terminal
form. materialize() installs one subscription on the root source and
returns a Cell<T, CellImmutable>.

No operators yet — next tasks move map/filter/try_map/tap/result_ext
onto Pipeline, replacing their Watchable<T>-bounded equivalents."
```

---

## Task 2: Port `map` onto `Pipeline`

**Files:**
- Modify: `hyphae/src/pipeline/map.rs` (replace placeholder)
- Modify: `hyphae/src/pipeline/mod.rs` (uncomment the `map` re-exports)
- Modify: `hyphae/src/lib.rs` (add `MapPipeline`, `MapExt` to re-exports)
- Delete: `hyphae/src/traits/operators/map.rs`
- Modify: `hyphae/src/traits/operators/mod.rs` (remove `mod map;` and `pub use map::MapExt;`)
- Modify: `hyphae/src/traits/mod.rs` (remove `MapExt` from the operators re-export list)
- Test: `hyphae/src/tests/pipeline_integration.rs`

- [ ] **Step 1: Write failing tests for `MapPipeline`**

Append to `hyphae/src/tests/pipeline_integration.rs`:

```rust
use crate::MapExt;

#[test]
fn map_pipeline_does_not_allocate_intermediate_cell() {
    let src = Cell::new(5).with_name("src");
    let pipeline = src.clone().map(|x| x * 2);
    // The pipeline is NOT a Cell — it has no subscribers table of its own.
    // Verify via shape: pipeline.get() must recompute from source.
    assert_eq!(Pipeline::get(&pipeline), 10);
    src.set(7);
    assert_eq!(Pipeline::get(&pipeline), 14);
}

#[test]
fn map_pipeline_materializes_to_subscribable_cell() {
    let src = Cell::new(3);
    let doubled: Cell<i32, _> = src.clone().map(|x| x * 2).materialize();
    assert_eq!(doubled.get(), 6);
    src.set(10);
    assert_eq!(doubled.get(), 20);
}

#[test]
fn map_pipeline_chains_fuse_into_one_subscription() {
    // Chain three maps. After materialize, the root source should have
    // exactly one subscriber installed by the pipeline (plus any test-owned
    // guards). Assert via DepNode::subscriber_count.
    use crate::traits::DepNode;

    let src = Cell::new(1).with_name("src");
    let initial_count = DepNode::subscriber_count(&src);

    let mat = src
        .clone()
        .map(|x| x + 1)
        .map(|x| x * 2)
        .map(|x| x + 10)
        .materialize();

    assert_eq!(DepNode::subscriber_count(&src), initial_count + 1,
        "chained pipeline must install exactly one subscription on root");
    assert_eq!(mat.get(), 14);
    src.set(5);
    assert_eq!(mat.get(), 22);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo flux run test -p hyphae -- pipeline_integration`
Expected: compile failure — `MapExt`, `map` method missing.

- [ ] **Step 3: Implement `MapPipeline`**

Replace `hyphae/src/pipeline/map.rs`:

```rust
//! Pure `map` operator as a zero-allocation pipeline.

use std::{marker::PhantomData, sync::Arc};

use crate::{
    pipeline::{Pipeline, PipelineInstall},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable},
};

/// Pipeline node representing `source.map(f)`. Does not allocate a cell.
pub struct MapPipeline<S, T, U, F> {
    source: S,
    f: Arc<F>,
    _types: PhantomData<fn(T) -> U>,
}

impl<S: Clone, T, U, F> Clone for MapPipeline<S, T, U, F> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            f: Arc::clone(&self.f),
            _types: PhantomData,
        }
    }
}

impl<S, T, U, F> Gettable<U> for MapPipeline<S, T, U, F>
where
    S: Gettable<T>,
    F: Fn(&T) -> U,
{
    fn get(&self) -> U {
        (self.f)(&self.source.get())
    }
}

impl<S, T, U, F> PipelineInstall<U> for MapPipeline<S, T, U, F>
where
    S: PipelineInstall<T> + Clone + Send + Sync + 'static,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> U + Send + Sync + 'static,
{
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<U>) + Send + Sync>,
    ) -> SubscriptionGuard {
        let f = Arc::clone(&self.f);
        // Wrap the downstream callback into an upstream callback that
        // transforms T -> U. `install` on the source recurses until it
        // hits the root cell, which subscribes to itself — result: one
        // fused closure on the root source.
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal: &Signal<T>| {
            match signal {
                Signal::Value(v) => {
                    let mapped = (f)(v.as_ref());
                    callback(&Signal::value(mapped));
                }
                Signal::Complete => callback(&Signal::Complete),
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            }
        });
        self.source.install(wrapped)
    }
}

impl<S, T, U, F> Pipeline<U> for MapPipeline<S, T, U, F>
where
    S: Pipeline<T>,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> U + Send + Sync + 'static,
{}

/// Extension trait adding `.map` to any `Pipeline<T>`.
pub trait MapExt<T: CellValue>: Pipeline<T> {
    #[track_caller]
    fn map<U, F>(self, f: F) -> MapPipeline<Self, T, U, F>
    where
        U: CellValue,
        F: Fn(&T) -> U + Send + Sync + 'static,
    {
        MapPipeline {
            source: self,
            f: Arc::new(f),
            _types: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T>> MapExt<T> for P {}
```

- [ ] **Step 4: Enable map re-export in `pipeline/mod.rs`**

Uncomment (or add, if you followed the placeholder path) in `pipeline/mod.rs`:

```rust
pub use map::{MapPipeline, MapExt};
```

And in `hyphae/src/lib.rs`, re-enable:

```rust
pub use pipeline::{Pipeline, MapPipeline, MapExt};
```

- [ ] **Step 5: Delete `hyphae/src/traits/operators/map.rs`**

```bash
rm hyphae/src/traits/operators/map.rs
```

- [ ] **Step 6: Remove `map` from `operators/mod.rs`**

Edit `hyphae/src/traits/operators/mod.rs`:

Remove line `mod map;` (around line 21) and `pub use map::MapExt;` (around line 63).

- [ ] **Step 7: Remove `MapExt` from `traits/mod.rs`**

Edit `hyphae/src/traits/mod.rs` — in the long `pub use operators::{...}` block (starts around line 21), remove the `MapExt,` entry.

- [ ] **Step 8: Run the test**

Run: `cargo flux run test -p hyphae -- pipeline_integration`
Expected: all three `map_pipeline_*` tests PASS.

- [ ] **Step 9: Run full check and test**

Run: `cargo flux run check` and `cargo flux run test -p hyphae`
Expected: all hyphae-internal code compiles. Some internal operators that used `.map(...)` in their impls (notably `tap.rs`, `result_ext.rs`, and anything else grep reveals) will break. **Do not fix those yet** — they are scheduled for their own tasks. If any non-scheduled code breaks, fix it minimally here.

Run: `grep -rn "\.map(" hyphae/src/traits/operators/ hyphae/src/traits/collections/ hyphae/src/`
Identify callers. Any that are NOT in files scheduled for deletion in later tasks need a `.materialize()` inserted now (or their upstream source needs to be a `Pipeline`, not a `Cell<...>::map` chain). If you find more than two such sites, add a "Task 2.5: fix internal map callers" subsection with exact file:line fixes and run this step again after.

- [ ] **Step 10: Commit**

```bash
git add hyphae/src/pipeline/map.rs hyphae/src/pipeline/mod.rs hyphae/src/lib.rs hyphae/src/traits/ hyphae/src/tests/
git rm hyphae/src/traits/operators/map.rs
git commit -m "feat(hyphae): port map onto Pipeline, remove MapExt on Watchable

map now returns MapPipeline<Source, T, U, F> instead of
Cell<U, CellImmutable>. Chained maps fuse into a single subscription
on the root source — verified by subscriber_count assertion.

Breaking: cell.map(f).subscribe(cb) no longer compiles. Callers must
insert .materialize() — cell.map(f).materialize().subscribe(cb)."
```

---

## Task 3: Port `filter` onto `Pipeline`

**Files:**
- Modify: `hyphae/src/pipeline/filter.rs`
- Modify: `hyphae/src/pipeline/mod.rs` (re-export)
- Modify: `hyphae/src/lib.rs` (re-export)
- Delete: `hyphae/src/traits/operators/filter.rs`
- Modify: `hyphae/src/traits/operators/mod.rs`
- Modify: `hyphae/src/traits/mod.rs`
- Test: `hyphae/src/tests/pipeline_integration.rs`

- [ ] **Step 1: Write the failing test**

Append to `pipeline_integration.rs`:

```rust
use crate::FilterExt;

#[test]
fn filter_pipeline_passes_matching_and_blocks_non_matching() {
    let src = Cell::new(10u64);
    let evens = src.clone().filter(|x| x % 2 == 0).materialize();

    // Matching initial value flows through.
    assert_eq!(evens.get(), 10);

    // Odd update: evens stays at 10.
    src.set(3);
    assert_eq!(evens.get(), 10);

    // Even update: evens moves.
    src.set(6);
    assert_eq!(evens.get(), 6);
}

#[test]
fn filter_pipeline_fuses_with_map() {
    use crate::MapExt;
    use crate::traits::DepNode;

    let src = Cell::new(1i64).with_name("src");
    let initial_count = DepNode::subscriber_count(&src);

    let out = src.clone()
        .map(|x| x + 10)
        .filter(|x| x % 2 == 0)
        .map(|x| x * 100)
        .materialize();

    assert_eq!(DepNode::subscriber_count(&src), initial_count + 1);
    // 1 + 10 = 11, odd, so filter blocks — materialized cell stays at initial
    // (which was computed from the current source value through the full chain).
    // Because filter blocks 11, the initial value installed in the cell at
    // materialize time is also 1+10=11 (pipeline.get() runs the full chain
    // without filter's block; see the filter get() semantics note below).
    // This is the "cold get" edge case documented in filter.rs.

    src.set(2); // 2 + 10 = 12, even, filter passes → out becomes 1200
    assert_eq!(out.get(), 1200);
}
```

- [ ] **Step 2: Run test, verify failure**

Run: `cargo flux run test -p hyphae -- pipeline_integration`
Expected: `FilterExt` / `filter` method missing.

- [ ] **Step 3: Implement `FilterPipeline`**

Replace `hyphae/src/pipeline/filter.rs`:

```rust
//! Pure `filter` operator as a zero-allocation pipeline.
//!
//! # Cold `get()` semantics
//!
//! `Pipeline::get()` recomputes through the chain on every call, ignoring
//! filter predicates — it has no prior value to fall back on when the
//! current source value fails the predicate. Callers who need "last-seen
//! value that passed" semantics must materialize: the materialized cell
//! caches the last emitted value and is not updated when the predicate
//! blocks. If the initial source value fails the predicate, the materialized
//! cell's initial value reflects the current source passed through the
//! pipeline's `get()` — which is NOT filter-aware. Document loudly.

use std::{marker::PhantomData, sync::Arc};

use crate::{
    pipeline::{Pipeline, PipelineInstall},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable},
};

pub struct FilterPipeline<S, T, P> {
    source: S,
    predicate: Arc<P>,
    _t: PhantomData<fn(T)>,
}

impl<S: Clone, T, P> Clone for FilterPipeline<S, T, P> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            predicate: Arc::clone(&self.predicate),
            _t: PhantomData,
        }
    }
}

impl<S, T, P> Gettable<T> for FilterPipeline<S, T, P>
where
    S: Gettable<T>,
    // Filter does NOT apply the predicate in get(). See module docs.
{
    fn get(&self) -> T {
        self.source.get()
    }
}

impl<S, T, P> PipelineInstall<T> for FilterPipeline<S, T, P>
where
    S: PipelineInstall<T> + Clone + Send + Sync + 'static,
    T: CellValue,
    P: Fn(&T) -> bool + Send + Sync + 'static,
{
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
        let predicate = Arc::clone(&self.predicate);
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal: &Signal<T>| {
            match signal {
                Signal::Value(v) => {
                    if (predicate)(v.as_ref()) {
                        callback(signal);
                    }
                }
                Signal::Complete => callback(&Signal::Complete),
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            }
        });
        self.source.install(wrapped)
    }
}

impl<S, T, P> Pipeline<T> for FilterPipeline<S, T, P>
where
    S: Pipeline<T>,
    T: CellValue,
    P: Fn(&T) -> bool + Send + Sync + 'static,
{}

pub trait FilterExt<T: CellValue>: Pipeline<T> {
    #[track_caller]
    fn filter<P>(self, predicate: P) -> FilterPipeline<Self, T, P>
    where
        P: Fn(&T) -> bool + Send + Sync + 'static,
    {
        FilterPipeline {
            source: self,
            predicate: Arc::new(predicate),
            _t: PhantomData,
        }
    }
}

impl<T: CellValue, Src: Pipeline<T>> FilterExt<T> for Src {}
```

- [ ] **Step 4: Wire re-exports**

In `hyphae/src/pipeline/mod.rs`:

```rust
pub use filter::{FilterPipeline, FilterExt};
```

In `hyphae/src/lib.rs`, add to the `pub use pipeline::{...}` block:

```rust
FilterPipeline, FilterExt,
```

- [ ] **Step 5: Delete old filter file**

```bash
rm hyphae/src/traits/operators/filter.rs
```

- [ ] **Step 6: Remove from operators modules**

Edit `hyphae/src/traits/operators/mod.rs`: remove `mod filter;` and `pub use filter::FilterExt;`.

Edit `hyphae/src/traits/mod.rs`: remove `FilterExt,` from the operators re-export.

- [ ] **Step 7: Run the tests**

Run: `cargo flux run test -p hyphae -- pipeline_integration`
Expected: both new filter tests PASS.

- [ ] **Step 8: Run full check**

Run: `cargo flux run check`
Expected: clean. Grep for `.filter(` outside the pipeline module and tests to confirm no broken internal callers. If any remain, fix minimally.

- [ ] **Step 9: Commit**

```bash
git add hyphae/src/pipeline/filter.rs hyphae/src/pipeline/mod.rs hyphae/src/lib.rs hyphae/src/traits/ hyphae/src/tests/
git rm hyphae/src/traits/operators/filter.rs
git commit -m "feat(hyphae): port filter onto Pipeline

filter now returns FilterPipeline instead of Cell<T, CellImmutable>.
get() on a FilterPipeline ignores the predicate (documented); only
the subscription path filters emissions. Materialize for filter-aware
caching behavior."
```

---

## Task 4: Port `try_map` onto `Pipeline`

**Files:**
- Modify: `hyphae/src/pipeline/try_map.rs`
- Modify: `hyphae/src/pipeline/mod.rs`, `hyphae/src/lib.rs`
- Delete: `hyphae/src/traits/operators/try_map.rs`
- Modify: `hyphae/src/traits/operators/mod.rs`, `hyphae/src/traits/mod.rs`
- Test: `hyphae/src/tests/pipeline_integration.rs`

- [ ] **Step 1: Failing test**

Append to `pipeline_integration.rs`:

```rust
use crate::TryMapExt;

#[test]
fn try_map_pipeline_produces_result_cell() {
    let src = Cell::new(10i32);
    let parsed = src.clone()
        .try_map(|v| if *v > 0 { Ok(v.to_string()) } else { Err("must be positive") })
        .materialize();

    assert_eq!(parsed.get(), Ok("10".to_string()));
    src.set(-5);
    assert_eq!(parsed.get(), Err("must be positive"));
}
```

- [ ] **Step 2: Run, verify failure**

Run: `cargo flux run test -p hyphae -- pipeline_integration`

- [ ] **Step 3: Implement `TryMapPipeline`**

Replace `hyphae/src/pipeline/try_map.rs`:

```rust
//! Pure `try_map` operator as a zero-allocation pipeline.

use std::{marker::PhantomData, sync::Arc};

use crate::{
    pipeline::{Pipeline, PipelineInstall},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable},
};

pub struct TryMapPipeline<S, T, U, E, F> {
    source: S,
    f: Arc<F>,
    _types: PhantomData<fn(T) -> Result<U, E>>,
}

impl<S: Clone, T, U, E, F> Clone for TryMapPipeline<S, T, U, E, F> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            f: Arc::clone(&self.f),
            _types: PhantomData,
        }
    }
}

impl<S, T, U, E, F> Gettable<Result<U, E>> for TryMapPipeline<S, T, U, E, F>
where
    S: Gettable<T>,
    F: Fn(&T) -> Result<U, E>,
{
    fn get(&self) -> Result<U, E> {
        (self.f)(&self.source.get())
    }
}

impl<S, T, U, E, F> PipelineInstall<Result<U, E>> for TryMapPipeline<S, T, U, E, F>
where
    S: PipelineInstall<T> + Clone + Send + Sync + 'static,
    T: CellValue,
    U: CellValue,
    E: CellValue,
    F: Fn(&T) -> Result<U, E> + Send + Sync + 'static,
{
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<Result<U, E>>) + Send + Sync>,
    ) -> SubscriptionGuard {
        let f = Arc::clone(&self.f);
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal: &Signal<T>| {
            match signal {
                Signal::Value(v) => callback(&Signal::value((f)(v.as_ref()))),
                Signal::Complete => callback(&Signal::Complete),
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            }
        });
        self.source.install(wrapped)
    }
}

impl<S, T, U, E, F> Pipeline<Result<U, E>> for TryMapPipeline<S, T, U, E, F>
where
    S: Pipeline<T>,
    T: CellValue,
    U: CellValue,
    E: CellValue,
    F: Fn(&T) -> Result<U, E> + Send + Sync + 'static,
{}

pub trait TryMapExt<T: CellValue>: Pipeline<T> {
    #[track_caller]
    fn try_map<U, E, F>(self, f: F) -> TryMapPipeline<Self, T, U, E, F>
    where
        U: CellValue,
        E: CellValue,
        F: Fn(&T) -> Result<U, E> + Send + Sync + 'static,
    {
        TryMapPipeline {
            source: self,
            f: Arc::new(f),
            _types: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T>> TryMapExt<T> for P {}
```

- [ ] **Step 4: Wire re-exports + delete old**

Same pattern as Task 2 — add `pub use try_map::{TryMapPipeline, TryMapExt};` in `pipeline/mod.rs` and `lib.rs`. Delete `hyphae/src/traits/operators/try_map.rs` and remove `mod try_map;` + `pub use try_map::TryMapExt;` from `operators/mod.rs` and `TryMapExt,` from `traits/mod.rs`.

- [ ] **Step 5: Run tests**

Run: `cargo flux run test -p hyphae -- pipeline_integration`
Expected: PASS.

- [ ] **Step 6: Run full check**

Run: `cargo flux run check`
Expected: clean. `try_map` has fewer dependents than `map`; should be trivial.

- [ ] **Step 7: Commit**

```bash
git add hyphae/src/pipeline/try_map.rs hyphae/src/pipeline/mod.rs hyphae/src/lib.rs hyphae/src/traits/ hyphae/src/tests/
git rm hyphae/src/traits/operators/try_map.rs
git commit -m "feat(hyphae): port try_map onto Pipeline"
```

---

## Task 5: Port `tap` onto `Pipeline`

**Files:**
- Modify: `hyphae/src/pipeline/tap.rs`
- Modify: `hyphae/src/pipeline/mod.rs`, `hyphae/src/lib.rs`
- Delete: `hyphae/src/traits/operators/tap.rs`
- Modify: `hyphae/src/traits/operators/mod.rs`, `hyphae/src/traits/mod.rs`
- Test: `hyphae/src/tests/pipeline_integration.rs`

- [ ] **Step 1: Failing test**

Append:

```rust
use crate::TapExt;
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn tap_pipeline_observes_without_modifying() {
    let src = Cell::new(0u64);
    let seen = Arc::new(AtomicU64::new(0));

    let seen_clone = Arc::clone(&seen);
    let mat = src.clone()
        .tap(move |v| { seen_clone.store(*v, Ordering::SeqCst); })
        .materialize();

    src.set(42);
    assert_eq!(seen.load(Ordering::SeqCst), 42);
    assert_eq!(mat.get(), 42);
}
```

Note: this test needs `use std::sync::Arc;` at the top of the tests file — add if missing.

- [ ] **Step 2: Implement `TapPipeline`**

Replace `hyphae/src/pipeline/tap.rs`:

```rust
//! Pure `tap` operator — side effect without transform.

use std::{marker::PhantomData, sync::Arc};

use crate::{
    pipeline::{Pipeline, PipelineInstall},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable},
};

pub struct TapPipeline<S, T, F> {
    source: S,
    f: Arc<F>,
    _t: PhantomData<fn(T)>,
}

impl<S: Clone, T, F> Clone for TapPipeline<S, T, F> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            f: Arc::clone(&self.f),
            _t: PhantomData,
        }
    }
}

impl<S, T, F> Gettable<T> for TapPipeline<S, T, F>
where
    S: Gettable<T>,
    F: Fn(&T),
{
    fn get(&self) -> T {
        let v = self.source.get();
        (self.f)(&v);
        v
    }
}

impl<S, T, F> PipelineInstall<T> for TapPipeline<S, T, F>
where
    S: PipelineInstall<T> + Clone + Send + Sync + 'static,
    T: CellValue,
    F: Fn(&T) + Send + Sync + 'static,
{
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
        let f = Arc::clone(&self.f);
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal: &Signal<T>| {
            if let Signal::Value(v) = signal {
                (f)(v.as_ref());
            }
            callback(signal);
        });
        self.source.install(wrapped)
    }
}

impl<S, T, F> Pipeline<T> for TapPipeline<S, T, F>
where
    S: Pipeline<T>,
    T: CellValue,
    F: Fn(&T) + Send + Sync + 'static,
{}

pub trait TapExt<T: CellValue>: Pipeline<T> {
    #[track_caller]
    fn tap<F>(self, f: F) -> TapPipeline<Self, T, F>
    where
        F: Fn(&T) + Send + Sync + 'static,
    {
        TapPipeline {
            source: self,
            f: Arc::new(f),
            _t: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T>> TapExt<T> for P {}
```

- [ ] **Step 3: Re-exports + delete old**

Add `pub use tap::{TapPipeline, TapExt};` in `pipeline/mod.rs` and to `lib.rs`.

Delete `hyphae/src/traits/operators/tap.rs`.

Remove `mod tap;` + `pub use tap::TapExt;` from `operators/mod.rs`. Remove `TapExt,` from `traits/mod.rs`.

- [ ] **Step 4: Run tests**

Run: `cargo flux run test -p hyphae -- pipeline_integration`
Expected: PASS.

- [ ] **Step 5: Run full check**

Run: `cargo flux run check`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add hyphae/src/pipeline/tap.rs hyphae/src/pipeline/mod.rs hyphae/src/lib.rs hyphae/src/traits/ hyphae/src/tests/
git rm hyphae/src/traits/operators/tap.rs
git commit -m "feat(hyphae): port tap onto Pipeline"
```

---

## Task 6: Port `result_ext` (`map_ok`, `map_err`, `catch_error`, `unwrap_or`, `unwrap_or_else`) onto `Pipeline`

These operators were defined on `Watchable<Result<T, E>>` in the old `result_ext.rs`. They all internally delegate to `map`, so the new versions delegate to `MapPipeline` through the pipeline chain.

**Files:**
- Modify: `hyphae/src/pipeline/result_ext.rs`
- Modify: `hyphae/src/pipeline/mod.rs`, `hyphae/src/lib.rs`
- Delete: `hyphae/src/traits/operators/result_ext.rs`
- Modify: `hyphae/src/traits/operators/mod.rs`, `hyphae/src/traits/mod.rs`
- Test: `hyphae/src/tests/pipeline_integration.rs`

- [ ] **Step 1: Failing tests**

Append:

```rust
use crate::{MapOkExt, MapErrExt, CatchErrorExt, UnwrapOrExt};

#[test]
fn map_ok_transforms_only_ok() {
    let src: Cell<Result<i32, String>> = Cell::new(Ok(5));
    let doubled = src.clone().map_ok(|v| v * 2).materialize();

    assert_eq!(doubled.get(), Ok(10));
    src.set(Err("boom".to_string()));
    assert_eq!(doubled.get(), Err("boom".to_string()));
}

#[test]
fn map_err_transforms_only_err() {
    let src: Cell<Result<i32, String>> = Cell::new(Err("oops".to_string()));
    let wrapped = src.clone().map_err(|e| format!("wrapped: {}", e)).materialize();

    assert_eq!(wrapped.get(), Err("wrapped: oops".to_string()));
    src.set(Ok(99));
    assert_eq!(wrapped.get(), Ok(99));
}

#[test]
fn catch_error_recovers() {
    let src: Cell<Result<i32, String>> = Cell::new(Err("bad".to_string()));
    let recovered = src.clone().catch_error(|_| 0i32).materialize();

    assert_eq!(recovered.get(), 0);
    src.set(Ok(42));
    assert_eq!(recovered.get(), 42);
}

#[test]
fn unwrap_or_provides_default() {
    let src: Cell<Result<i32, String>> = Cell::new(Err("bad".to_string()));
    let unwrapped = src.clone().unwrap_or(-1i32).materialize();

    assert_eq!(unwrapped.get(), -1);
    src.set(Ok(77));
    assert_eq!(unwrapped.get(), 77);
}
```

- [ ] **Step 2: Implement `pipeline/result_ext.rs`**

Replace `hyphae/src/pipeline/result_ext.rs`:

```rust
//! Result-typed pipeline operators: map_ok, map_err, catch_error, unwrap_or.
//!
//! These wrap `MapExt::map` with fixed per-variant closures. They
//! are pure transformations and allocate no intermediate cells.

use crate::{
    pipeline::{Pipeline, MapPipeline, MapExt},
    traits::CellValue,
};

// Concrete type aliases so users can name pipeline outputs in signatures
// without writing the full closure type.

/// Pipeline type produced by `.map_ok(f)`.
pub type MapOkPipeline<S, T, U, E, F> = MapPipeline<
    S,
    Result<T, E>,
    Result<U, E>,
    MapOkClosure<T, U, E, F>,
>;

/// Closure adapter invoked inside `MapPipeline`. Named to expose a type
/// name users can spell if needed. Not for external construction.
pub struct MapOkClosure<T, U, E, F> {
    _p: std::marker::PhantomData<(T, U, E, F)>,
}

// The actual extension trait re-uses `MapExt::map` under the hood —
// users call `.map_ok(f)`, which builds a `MapPipeline` with a wrapping
// closure. Implementation uses `impl Fn` so no `MapOkClosure` struct is
// actually instantiated; the type alias above is aspirational and may be
// simplified to just re-exporting `MapPipeline` if concrete naming is
// unnecessary.

pub trait MapOkExt<T: CellValue, E: CellValue>: Pipeline<Result<T, E>> {
    #[track_caller]
    fn map_ok<U, F>(
        self,
        f: F,
    ) -> MapPipeline<
        Self,
        Result<T, E>,
        Result<U, E>,
        impl Fn(&Result<T, E>) -> Result<U, E> + Send + Sync + 'static,
    >
    where
        U: CellValue,
        F: Fn(&T) -> U + Send + Sync + 'static,
    {
        self.map(move |r: &Result<T, E>| match r {
            Ok(v) => Ok(f(v)),
            Err(e) => Err(e.clone()),
        })
    }
}

impl<T: CellValue, E: CellValue, P: Pipeline<Result<T, E>>> MapOkExt<T, E> for P {}

pub trait MapErrExt<T: CellValue, E: CellValue>: Pipeline<Result<T, E>> {
    #[track_caller]
    fn map_err<E2, F>(
        self,
        f: F,
    ) -> MapPipeline<
        Self,
        Result<T, E>,
        Result<T, E2>,
        impl Fn(&Result<T, E>) -> Result<T, E2> + Send + Sync + 'static,
    >
    where
        E2: CellValue,
        F: Fn(&E) -> E2 + Send + Sync + 'static,
    {
        self.map(move |r: &Result<T, E>| match r {
            Ok(v) => Ok(v.clone()),
            Err(e) => Err(f(e)),
        })
    }
}

impl<T: CellValue, E: CellValue, P: Pipeline<Result<T, E>>> MapErrExt<T, E> for P {}

pub trait CatchErrorExt<T: CellValue, E: CellValue>: Pipeline<Result<T, E>> {
    #[track_caller]
    fn catch_error<F>(
        self,
        f: F,
    ) -> MapPipeline<
        Self,
        Result<T, E>,
        T,
        impl Fn(&Result<T, E>) -> T + Send + Sync + 'static,
    >
    where
        F: Fn(&E) -> T + Send + Sync + 'static,
    {
        self.map(move |r: &Result<T, E>| match r {
            Ok(v) => v.clone(),
            Err(e) => f(e),
        })
    }
}

impl<T: CellValue, E: CellValue, P: Pipeline<Result<T, E>>> CatchErrorExt<T, E> for P {}

pub trait UnwrapOrExt<T: CellValue, E: CellValue>: Pipeline<Result<T, E>> {
    #[track_caller]
    fn unwrap_or(
        self,
        default: T,
    ) -> MapPipeline<
        Self,
        Result<T, E>,
        T,
        impl Fn(&Result<T, E>) -> T + Send + Sync + 'static,
    > {
        self.map(move |r: &Result<T, E>| match r {
            Ok(v) => v.clone(),
            Err(_) => default.clone(),
        })
    }

    #[track_caller]
    fn unwrap_or_else<F>(
        self,
        f: F,
    ) -> MapPipeline<
        Self,
        Result<T, E>,
        T,
        impl Fn(&Result<T, E>) -> T + Send + Sync + 'static,
    >
    where
        F: Fn(&E) -> T + Send + Sync + 'static,
    {
        self.catch_error(f)
    }
}

impl<T: CellValue, E: CellValue, P: Pipeline<Result<T, E>>> UnwrapOrExt<T, E> for P {}

// NOTE(ts): the standalone `MapOkPipeline`/`MapErrPipeline`/... type aliases
// above are placeholders. Since `impl Fn` is used in signatures, the actual
// concrete type produced is unnameable to consumers — rename these aliases
// to `_unused` or remove entirely during Task 6 cleanup if no caller needs them.
```

Then in `pipeline/mod.rs`, simplify the re-exports:

```rust
pub use result_ext::{
    MapOkExt, MapErrExt, CatchErrorExt, UnwrapOrExt,
};
```

(The `MapOkPipeline`, etc. type aliases are not re-exported — their `impl Fn` return types are unnameable anyway.)

In `lib.rs`, update the `pub use pipeline::{...}` block to match:

```rust
pub use pipeline::{
    Pipeline,
    MapPipeline, MapExt,
    FilterPipeline, FilterExt,
    TryMapPipeline, TryMapExt,
    TapPipeline, TapExt,
    MapOkExt, MapErrExt, CatchErrorExt, UnwrapOrExt,
};
```

- [ ] **Step 3: Delete old `result_ext.rs`**

```bash
rm hyphae/src/traits/operators/result_ext.rs
```

- [ ] **Step 4: Remove from operators modules**

Edit `hyphae/src/traits/operators/mod.rs`: remove `mod result_ext;` and the corresponding `pub use result_ext::{CatchErrorExt, MapErrExt, MapOkExt, UnwrapOrExt};`.

Edit `hyphae/src/traits/mod.rs`: in the long `pub use operators::{...}` line, remove `CatchErrorExt,`, `MapErrExt,`, `MapOkExt,`, `UnwrapOrExt,`.

- [ ] **Step 5: Run tests**

Run: `cargo flux run test -p hyphae -- pipeline_integration`
Expected: the four new tests PASS.

- [ ] **Step 6: Run full check**

Run: `cargo flux run check`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add hyphae/src/pipeline/result_ext.rs hyphae/src/pipeline/mod.rs hyphae/src/lib.rs hyphae/src/traits/ hyphae/src/tests/
git rm hyphae/src/traits/operators/result_ext.rs
git commit -m "feat(hyphae): port map_ok/map_err/catch_error/unwrap_or onto Pipeline"
```

---

## Task 7: Audit `cold` and `finalize`, decide placement

**Files:**
- Read: `hyphae/src/traits/operators/cold.rs`
- Read: `hyphae/src/traits/operators/finalize.rs`
- Possibly modify: move to pipeline or leave
- Modify: `hyphae/src/traits/operators/mod.rs`
- Test: may not need test changes

Read both files first. Decide:

- [ ] **Step 1: Read `cold.rs`**

Run: `Read hyphae/src/traits/operators/cold.rs`

Evaluate: does `cold` transform signals statelessly, or does it maintain a flag (e.g., "have we emitted the initial value")? If stateless transform — port to `pipeline/cold.rs` following Task 2's pattern. If stateful — leave as a `Cell`-producing operator.

- [ ] **Step 2: Read `finalize.rs`**

Run: `Read hyphae/src/traits/operators/finalize.rs`

Evaluate: `finalize` typically runs a cleanup callback on Complete/Error. It can be expressed as a pure transformation on signals (forward Value, intercept Complete/Error). Candidate for pipeline port.

- [ ] **Step 3: Port if stateless**

For each that is stateless, follow the Task 2 pattern — create `hyphae/src/pipeline/cold.rs` / `finalize.rs`, delete the old operators/*.rs file, re-export.

For each that is stateful or synchronizes, leave as-is but verify it still compiles now that `Cell` no longer has `map`/`filter` directly. It should, because `Cell: Pipeline<T>` lets stateful operators depend on `Watchable<T>` independently.

- [ ] **Step 4: Run tests**

Run: `cargo flux run test -p hyphae`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore(hyphae): audit cold/finalize — [ported | left as Watchable ops]"
```

(Edit commit message to match what you did.)

---

## Task 8: Fix internal hyphae callers broken by the `Watchable -> Pipeline` migration

**Files:**
- Grep target: `hyphae/src/**`

Some internal operators or collections may chain `.map(...)` or `.filter(...)` directly on a `Cell` and then `.subscribe(...)`. Those break under the new API. Find and fix.

- [ ] **Step 1: Grep for direct chains**

Run: `grep -rn "\.map(.*\.subscribe\|\.filter(.*\.subscribe\|\.try_map(.*\.subscribe\|\.tap(.*\.subscribe" hyphae/src/`

For each hit that isn't a test or a file already ported:
- If the code is in an operator impl that still produces a `Cell`, insert `.materialize()` between the pure op and the stateful code.
- If the code is building up values for storage, consider whether it should be a pipeline all the way through and only materialize at the leaf.

- [ ] **Step 2: Also grep for method-chain `.map().<some Cell-only method>`**

Run: `grep -rn "\.map(.*)\.with_name\|\.map(.*)\.subscribe_result\|\.map(.*)\.dependency_" hyphae/src/`

These rely on methods that exist on `Cell` but not `Pipeline`. Insert `.materialize()`.

- [ ] **Step 3: Fix each hit**

Work through the list mechanically. Common patterns:

Before:
```rust
let derived = source.map(|x| transform(x)).with_name("derived");
derived.subscribe(|sig| { ... });
```

After:
```rust
let derived = source.map(|x| transform(x)).materialize().with_name("derived");
derived.subscribe(|sig| { ... });
```

- [ ] **Step 4: Run tests**

Run: `cargo flux run test -p hyphae`
Expected: all PASS.

- [ ] **Step 5: Run clippy**

Check `.bacon-locations` for pre-existing errors first:

Run: `cat .bacon-locations 2>/dev/null | head -20`

Then: `cargo flux run lint -p hyphae`
Expected: no new clippy warnings beyond pre-existing. Fix any new warnings you introduced.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "fix(hyphae): insert .materialize() at internal Cell/Pipeline boundaries

Internal callers of .map/.filter/.try_map/.tap/.map_ok/etc. now receive
pipelines instead of cells. Insert .materialize() where the downstream
code requires Cell-only APIs (subscribe, with_name, subscribe_result)."
```

---

## Task 9: Audit rship blast radius

**Files:**
- External grep target: `/home/trevor/Code/rship/**`

This task is *measurement only*. It does not fix rship; it reports the cost of the upgrade.

- [ ] **Step 1: Count candidate break sites**

Run from `/home/trevor/Code/rship/`:

```bash
grep -rn "\.map(.*\.subscribe\|\.filter(.*\.subscribe\|\.map_ok(.*\.subscribe\|\.try_map(.*\.subscribe\|\.tap(.*\.subscribe" libs/ apps/ --include="*.rs" | wc -l
```

Also run each pattern alone to get per-operator counts:

```bash
grep -rn "\.map(.*\.subscribe" libs/ apps/ --include="*.rs" | wc -l
grep -rn "\.filter(.*\.subscribe" libs/ apps/ --include="*.rs" | wc -l
grep -rn "\.try_map(.*\.subscribe" libs/ apps/ --include="*.rs" | wc -l
grep -rn "\.tap(.*\.subscribe" libs/ apps/ --include="*.rs" | wc -l
grep -rn "\.map_ok(.*\.subscribe\|\.map_err(.*\.subscribe\|\.catch_error(.*\.subscribe\|\.unwrap_or(.*\.subscribe" libs/ apps/ --include="*.rs" | wc -l
```

Also look for uses of `Cell` in signatures that would become `Pipeline`:

```bash
grep -rn "fn.*-> Cell<.*, CellImmutable>" libs/ apps/ --include="*.rs" | grep -E "\.map|\.filter|\.try_map|\.tap" | wc -l
```

- [ ] **Step 2: Sample 5-10 call sites across libs**

Pick at random from the grep output. Read each site. Confirm the fix is "insert `.materialize()` before subscribe/with_name/etc." — not a deeper restructuring. Note any site that requires deeper thinking.

- [ ] **Step 3: Write the audit report**

Create `hyphae/docs/superpowers/plans/2026-04-23-pipeline-type-rship-audit.md`:

```markdown
# rship Blast Radius for Pipeline Migration

Grep counts (date: YYYY-MM-DD):
- `.map(...).subscribe(...)`: N sites
- `.filter(...).subscribe(...)`: N sites
- `.try_map(...).subscribe(...)`: N sites
- `.tap(...).subscribe(...)`: N sites
- `.map_ok` / `.map_err` / `.catch_error` / `.unwrap_or` + subscribe: N sites
- Functions returning `Cell<_, CellImmutable>` via pure ops: N sites

Total candidate fix sites: N.

## Sampled call sites

1. `<path>:<line>` — description, fix plan
2. ...

## Non-trivial sites

(List any that require more than just `.materialize()` insertion.)
```

- [ ] **Step 4: Commit audit**

```bash
git add hyphae/docs/superpowers/plans/2026-04-23-pipeline-type-rship-audit.md
git commit -m "docs(hyphae): audit rship blast radius for Pipeline migration"
```

---

## Task 10: Update hyphae public docs

**Files:**
- Modify: `hyphae/src/lib.rs` (top-level doc comment)
- Modify: `hyphae/README.md`

- [ ] **Step 1: Update `lib.rs` top-level docs**

Edit the doc comment block at the top of `hyphae/src/lib.rs` (lines 1-57). Replace the Quick Start to show the new API:

```rust
//! ## Quick Start
//!
//! ```rust
//! use hyphae::{Cell, MapExt, Mutable, Watchable, JoinExt, Pipeline, Signal, flat};
//!
//! // Create reactive cells
//! let x = Cell::new(5).with_name("x");
//! let y = Cell::new(10).with_name("y");
//!
//! // Pure operators return pipelines — no intermediate allocation
//! let doubled = x.clone().map(|val| val * 2);
//!
//! // Stateful operators (like `join`) or materialize() produce cells
//! let sum = x.clone().join(&y).map(flat!(|a, b| a + b)).materialize();
//!
//! // Subscribe on the materialized cell
//! let _guard = sum.subscribe(|signal| {
//!     if let Signal::Value(value) = signal {
//!         println!("Sum changed to: {}", value);
//!     }
//! });
//!
//! x.set(20); // Triggers updates
//! ```
//!
//! ## Pipelines vs Cells
//!
//! Pure operators (`map`, `filter`, `try_map`, `tap`, `map_ok`, etc.)
//! return a [`Pipeline`] — an uncompiled operation chain with no subscriber
//! machinery. Chaining pipelines fuses closures at compile time; the fused
//! closure only runs when a consumer calls [`Pipeline::materialize`].
//!
//! [`Cell`] is the materialized, cached, multicast form. Subscribing requires
//! a cell — there is no `Pipeline::subscribe` by design, forcing callers to
//! make the memoization decision explicit.
```

- [ ] **Step 2: Update README**

Edit `hyphae/README.md` — update any code examples that use `.map(...).subscribe(...)` to insert `.materialize()`. If the README has a "concepts" section, add a short "Pipelines" subsection.

- [ ] **Step 3: Verify doc examples compile**

Run: `cargo flux run test -p hyphae --doc`
Expected: all doctests PASS. Fix any that don't.

- [ ] **Step 4: Commit**

```bash
git add hyphae/src/lib.rs hyphae/README.md
git commit -m "docs(hyphae): update Quick Start and README for Pipeline API"
```

---

## Task 11: Add targeted micro-benchmark confirming the perf claim

**Files:**
- Create: `hyphae/benches/pipeline_fusion.rs`
- Modify: `hyphae/Cargo.toml` (register the new bench)

- [ ] **Step 1: Inspect existing benches for style**

Run: `ls hyphae/benches/ && head -40 hyphae/benches/*.rs`

- [ ] **Step 2: Create the bench file**

Create `hyphae/benches/pipeline_fusion.rs`:

```rust
//! Micro-benchmark confirming that pipeline chains fuse into a single
//! subscription and do not allocate intermediate cells.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use hyphae::{Cell, MapExt, Mutable, Pipeline};

fn bench_map_chain_cells(c: &mut Criterion) {
    // Baseline: simulate old behavior by materializing between every map.
    c.bench_function("map_chain_materialized_3x", |b| {
        let src = Cell::new(0i64);
        let out = src.clone()
            .map(|x| x + 1).materialize()
            .map(|x| x * 2).materialize()
            .map(|x| x - 3).materialize();
        b.iter(|| {
            src.set(black_box(42));
            black_box(&out);
        });
    });
}

fn bench_map_chain_pipeline(c: &mut Criterion) {
    // New behavior: one materialize at the end, all maps fused.
    c.bench_function("map_chain_fused_3x", |b| {
        let src = Cell::new(0i64);
        let out = src.clone()
            .map(|x| x + 1)
            .map(|x| x * 2)
            .map(|x| x - 3)
            .materialize();
        b.iter(|| {
            src.set(black_box(42));
            black_box(&out);
        });
    });
}

criterion_group!(benches, bench_map_chain_cells, bench_map_chain_pipeline);
criterion_main!(benches);
```

- [ ] **Step 3: Register the bench**

Add to `hyphae/Cargo.toml`:

```toml
[[bench]]
name = "pipeline_fusion"
harness = false
```

(Under existing `[[bench]]` entries. If none exist and criterion isn't a dev-dep yet, add `criterion` to `[dev-dependencies]`.)

- [ ] **Step 4: Run the bench**

Run: `cargo bench --bench pipeline_fusion --target-dir target/claude`
Expected: `map_chain_fused_3x` at least 2x faster than `map_chain_materialized_3x` on per-emit throughput. If not, investigate — the fusion isn't working.

- [ ] **Step 5: Commit**

```bash
git add hyphae/benches/pipeline_fusion.rs hyphae/Cargo.toml
git commit -m "bench(hyphae): compare fused pipeline vs materialized cell chain"
```

---

## Spec Coverage Check

Before calling this spec done, verify every design point from the brainstorm maps to a task:

- [x] `Pipeline<T>` trait without `subscribe` — Task 1
- [x] `PipelineInstall<T>` crate-private installer — Task 1
- [x] `materialize(self) -> Cell<T, CellImmutable>` — Task 1
- [x] `Cell<T, M>: Pipeline<T>` blanket impl — Task 1
- [x] Chained pipelines fuse into one root subscription — Task 2 (test in Step 1)
- [x] `map` ported — Task 2
- [x] `filter` ported — Task 3
- [x] `try_map` ported — Task 4
- [x] `tap` ported — Task 5
- [x] `map_ok` / `map_err` / `catch_error` / `unwrap_or` ported — Task 6
- [x] `cold` / `finalize` decision — Task 7
- [x] Internal hyphae callers fixed — Task 8
- [x] rship blast-radius audit — Task 9
- [x] Docs updated — Task 10
- [x] Benchmark confirming fusion wins — Task 11

## Out of Scope (explicitly deferred)

- **Porting rship to the new API.** Audited in Task 9, not executed. The break is mechanical (insert `.materialize()`); schedule a follow-up branch after this lands.
- **Stateful operators.** `scan`, `debounce`, `throttle`, `buffer_*`, `pairwise`, `window`, `distinct_until_changed`, `state_transition`, `sample`, `delay`, `take`, `first`, `last`, `join`, `with_latest_from`, `zip`, `merge`, `merge_map`, `switch_map` all remain `Watchable<T>`-bounded + `Cell<U, CellImmutable>`-producing. Callers who want `pipeline.scan(...)` must write `pipeline.materialize().scan(...)`.
- **Alternate materialization strategies.** `.share()` (reference-counted materialization), `.hot()`, etc. are future work. `materialize(self)` is single-shot; clone the pipeline first if you need multiple materializations.
- **Metrics / tracing inside pipelines.** The current `Cell` metrics infrastructure only activates when a cell exists. Pipeline operators run inside one fused closure on the root cell, so their individual timings collapse into that cell's slow-subscriber metric. This is likely desirable (fewer metrics to aggregate) but document the limitation in the next release notes.
- **`subscribe_result` for pipelines.** Pipelines don't subscribe at all; callers who want result-subscribe semantics materialize first.
- **`WeakPipeline`.** No downgrade/upgrade mechanism. Pipelines are cheap to clone (just `Arc` bumps for closures and the source); if needed later, add then.

## Self-Review Notes

**Placeholder scan:** No `TODO`, `TBD`, `implement later`, or "appropriate error handling" appear in actual Task steps. The intentional placeholders in Task 1 Step 6 are scoped to one intermediate state and replaced by Task 2 Step 4.

**Type consistency:**
- `Pipeline<T>` — consistent throughout.
- `PipelineInstall<T>` — consistent throughout; crate-private (`pub(crate)`).
- Operator extension trait names (`MapExt`, `FilterExt`, `TryMapExt`, `TapExt`, `MapOkExt`, `MapErrExt`, `CatchErrorExt`, `UnwrapOrExt`) and method names are kept identical to today's API — only the supertrait bound changes from `Watchable<T>` to `Pipeline<T>`. The traits themselves move from `traits/operators/` to `pipeline/`, and the old definitions are deleted in the same task that introduces the replacements.
- `materialize` — one spelling, one definition in Task 1.
- Concrete struct names `MapPipeline`, `FilterPipeline`, `TryMapPipeline`, `TapPipeline` — consistent.

**Ordering hazard:** Within each port task (Tasks 2–6), there is an intermediate state — between adding the new `pub use pipeline::MapExt` in `lib.rs` and removing the old `pub use traits::operators::MapExt` — where the crate has two `MapExt` re-exports at the same path. Apply *all* edits in the task before running `cargo flux run check`; do not check between steps. The final state is unambiguous (only `pipeline::MapExt` remains).

**Spec coverage:** Every point above has a task. No gaps.

**Known friction point to flag in review:** Task 6's `impl Fn` return types make the concrete `MapOkPipeline` etc. type aliases unnameable in user code. That's acceptable for ergonomic use (`.map_ok(f).materialize()`) but if a user needs to *store* a pipeline in a struct field, they'll hit it. The escape hatch is to materialize sooner. Document in Task 6's commit message and in `lib.rs` module docs.

---

## Execution Handoff

Plan complete and saved to `hyphae/docs/superpowers/plans/2026-04-23-pipeline-type.md`.

Two execution options:

1. **Subagent-Driven (recommended)** — dispatch a fresh subagent per task, review between tasks, fast iteration.
2. **Inline Execution** — execute tasks in this session using executing-plans, batch execution with checkpoints.

**If resuming later**, the entry point is Task 1. Tasks are strictly sequential; do not attempt to start mid-plan without having completed earlier tasks, since each task deletes an old file while replacing it from the new module.
