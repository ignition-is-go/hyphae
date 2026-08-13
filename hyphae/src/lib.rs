//! # hyphae - High-Performance Reactive Programming Library
//!
//! A high-performance, type-safe concurrent reactive programming library with
//! heterogeneous cell combinations and comprehensive dependency tracking.
//!
//! ## Features
//!
//! - **Fast Reads**: Uses `arc-swap` for atomic value snapshots
//! - **Type-Safe**: Full compile-time type checking with heterogeneous cell support
//! - **Automatic Propagation**: Changes flow through dependency chains automatically
//! - **Dependency Tracking**: Inspect and visualize cell relationships
//! - **Thread-Safe**: Safe concurrent access across threads
//!
//! ## Quick Start
//!
//! ```rust
//! use hyphae::{Cell, MapExt, Materialize, Mutable, Watchable, JoinExt, Pipeline, Signal, flat};
//!
//! // Create reactive cells
//! let x = Cell::new(5).with_name("x");
//! let y = Cell::new(10).with_name("y");
//!
//! // Pure operators (map/filter/...) return pipelines — no allocation
//! // until you materialize.
//! let doubled = x.clone().map(|val| val * 2).materialize().with_name("doubled");
//!
//! // Combine multiple cells with join + flat!. join stays lazy, then creates
//! // its required fan-in coalescing boundary when the chain is materialized.
//! let sum = x.clone().join(y).map(flat!(|a, b| a + b)).materialize().with_name("sum");
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
//! Pure operators (`map`, `filter`, `try_map`, `tap`, `map_ok`, `map_err`,
//! `catch_error`, `unwrap_or`) return a [`Pipeline`] — an uncompiled chain
//! that has not yet been materialized into a [`Cell`]. Chaining pipelines
//! fuses closures at compile time; the fused closure runs only when a
//! consumer calls [`Materialize::materialize`].
//!
//! [`Cell`] is the materialized, cached, multicast form. Subscribing requires
//! a cell — there is no `Pipeline::subscribe` by design, forcing callers to
//! make the memoization decision explicit.
//! [`Definite`] pipelines materialize to `Cell<T>`; [`Empty`] pipelines such
//! as `filter` materialize to `Cell<Option<T>>` because they may not have an
//! honest initial value.
//!
//! Operators that need state or multiple sources (`debounce`, `buffer_*`,
//! `join`, `merge`, `switch_map`, and others) are lazy pipelines too. Their
//! state and any required fan-in boundary are created only when the pipeline
//! is installed. Materialize at the point where a cached value, [`Gettable`],
//! or [`Watchable`] boundary is required.
//!
//! Derived collection views (`CellMap::get`, `entries`, `items`, `keys`,
//! `size`, `len`, `diffs`, and their `CellSet` counterparts) also expose
//! definite pipelines. Some reuse an internal cell today, making terminal
//! materialization a no-op, but callers cannot rely on that implementation
//! detail as an implicit observation boundary.
//!
//! See the [Hyphae 3.0 migration guide](https://github.com/ignition-is-go/hyphae/blob/main/docs/migrating-to-v3.md)
//! for owned-input and sharing examples.
//!
//! ## Combining Cells
//!
//! Use `join()` to combine cells, and the `flat!` macro to avoid nested tuple destructuring.
//! `join` consumes its inputs into a lazy pipeline. It creates its required
//! fan-in coalescing boundary only when the chain is materialized:
//!
//! ```rust
//! use hyphae::{Cell, Gettable, JoinExt, MapExt, Materialize, flat};
//!
//! let a = Cell::new(1);
//! let b = Cell::new(2);
//! let c = Cell::new(3);
//! let d = Cell::new(4);
//!
//! // Without flat!: |(((a, b), c), d)| - deeply nested
//! // With flat!: |a, b, c, d| - clean and simple
//! let sum = a
//!     .join(b)
//!     .materialize()
//!     .join(c)
//!     .materialize()
//!     .join(d)
//!     .map(flat!(|a, b, c, d| a + b + c + d))
//!     .materialize();
//! assert_eq!(sum.get(), 10);
//! ```
//!
//! ## Map Queries vs `CellMap`s
//!
//! Pure [`CellMap`] operators return consuming, non-`Clone` [`MapQuery`] plans.
//! [`MapQuery`] exposes associated [`MapQuery::Key`] and [`MapQuery::Value`]
//! types. Semantic operators state their cardinality and key behavior:
//! `select`/`select_by`, `map_values`/`filter_map_values`,
//! `map_entries`/`filter_map_entries`, and `flat_map_entries`.
//!
//! Plans compile to a statically typed, monomorphized runtime. Recognized
//! key-preserving and join regions fuse; rekeys and unsupported shapes remain
//! physical boundaries. No intermediate *observable* `CellMap` is created.
//! [`MapQuery::materialize`] is the sole observation boundary: it consumes the
//! plan, installs one subscription per interned physical root, and returns the
//! cached output map. Materialize once and clone that map to share work.
//!
//! Named zero-sized [`ForeignKeyRelation`] markers give typed FK joins their
//! semantic relationship, partition, and index identity. Repeated uses of one
//! raw physical right source and relation share an index within a materialized
//! plan; transformed rights intentionally do not alias it.
//!
//! Query closures must be deterministic, externally side-effect-free, and
//! nonblocking. They may run repeatedly or concurrently, and their invocation
//! count, order, and thread are not API guarantees. Output publication remains
//! deterministic, ordered, and synchronously settled. See [`map_query`] for
//! exact execution, collision, teardown, completion/error, and panic contracts.
//!
//! Native builds with the `scheduler` feature may adaptively dispatch eligible
//! join-region work to Hyphae's shared dedicated worker pool. Wasm and builds
//! without that feature execute map queries sequentially.
//!
//! ## `CellMap` Quick Start
//!
//! ```rust
//! use hyphae::{CellMap, MapQuery, traits::{InnerJoinExt, MapValuesExt}};
//!
//! let users = CellMap::<String, &'static str>::new();
//! let scores = CellMap::<String, i32>::new();
//! users.insert("u1".into(), "alice");
//! scores.insert("u1".into(), 42);
//!
//! // Chained operators return plan nodes — no intermediate CellMap
//! // until materialize().
//! let view = users
//!     .clone()
//!     .inner_join(scores.clone())
//!     .map_values(|_, (name, score)| format!("{name}:{score}"))
//!     .materialize();
//!
//! assert!(view.contains_key(&"u1".to_string()));
//! ```

#[macro_use]
pub mod flat;
#[cfg(feature = "async")]
pub mod async_support;
pub mod bounded_input;
pub mod bounded_output;
pub mod cell;
pub mod cell_map;
pub mod cell_set;
#[cfg(feature = "scheduler")]
pub mod clock;
pub mod constructors;
#[cfg(feature = "scheduler")]
pub(crate) mod executor;
pub mod map_query;
pub mod nested_map;
pub mod pipeline;
pub(crate) mod platform;
#[cfg(feature = "profiling")]
pub mod profiling;
#[cfg(feature = "region-calibration")]
pub mod region_calibration;
#[cfg(feature = "scheduler")]
pub mod scheduler;
pub mod signal;
pub mod source;
pub mod subscription;
pub mod traits;

// Both are available on wasm: the registry is fully portable; the `server`
// module keeps a uniform public API but its TCP transport (tokio/mio) is
// native-only, so on wasm `start_server` returns an inert handle.

#[cfg(test)]
mod tests;

#[cfg(feature = "async")]
pub use async_support::{AsyncWatchableExt, CellStream};
pub use bounded_input::{BoundedInput, BoundedInputMetrics, OverflowPolicy};
pub use bounded_output::BoundedOutput;
pub use cell::{Cell, CellImmutable, CellMutable};
pub use cell_map::{CellMap, MapDiff, WeakCellMap};
pub use cell_set::{CellSet, SetDiff};
#[cfg(feature = "scheduler")]
pub use clock::{Clock, IntervalTickSource, MonotonicClock, Tick, TickGuard, TickSource};
pub use constructors::{
    IntervalTick, from_iter_with_delay, interval, interval_precise, interval_precise_source,
    interval_precise_with_elapsed, interval_precise_with_elapsed_source, interval_source,
};
pub use map_query::{MapQuery, MapQueryShareExt, SharedMapQuery};
pub use nested_map::NestedMap;
pub use pipeline::{
    Definite, Empty, Materialize, Pipeline, PipelineShareExt, Seedness, SharedPipeline,
};
#[cfg(feature = "scheduler")]
pub use scheduler::batch;
pub use signal::Signal;
pub use source::{SampleOnSourceExt, Source, WeakSource};
pub use subscription::SubscriptionGuard;
pub use traits::{
    AuditExt, BackpressureExt, BufferCountExt, BufferTimeExt, CatchErrorExt, CellValue, ColdExt,
    CollectProject, ConcatExt, CountByExt, DebounceExt, DedupedExt, DelayExt, DepNode,
    DirectJoinProjection, DirectProject, DistinctExt, DistinctUntilChangedByExt, FilterExt,
    FilterMapValuesPlan, FinalizeExt, FirstExt, FlatMapEntriesExt, ForeignKeyRelation, Gettable,
    GroupByExt, IdFor, IdType, InnerJoinExt, JCons, JNil, JoinExt, JoinKeyFrom, JoinProjection,
    JoinRegion, JoinStage, JoinedValuesPlan, KeyChange, LastExt, LastStage, LeftJoinExt,
    LeftJoinPlan, LeftSemiJoinExt, MapEntriesExt, MapErrExt, MapExt, MapLast, MapOkExt,
    MapValuesExt, MapValuesPlan, MergeExt, MergeMapExt, MultiLeftJoinExt, Mutable,
    OptionalRightKey, OwnedIndex, PairwiseExt, ParallelCell, ParallelExt, ProjectCellExt, Push,
    ReactiveKeys, ReactiveMap, RelationPlan, ReplaceLastProject, RequiredRightKey, RetryExt,
    RightJoinKey, SampleExt, ScanExt, SelectCellExt, SelectExt, SharedRelationIndex, SkipExt,
    SkipWhileExt, StageList, StateMachineBuilder, StateTransitionExt, SwitchMapExt, TakeExt,
    TakeUntilExt, TakeWhileExt, TapExt, ThenMap, ThrottleExt, TimeoutExt, TryMapExt,
    TupleJoinProjection, TwoLeftJoinMappedPlan, TwoLeftJoinPlan, UnwrapOrExt, Watchable,
    WatchableResult, WindowExt, WithLatestFromExt, ZipExt, join_vec,
};
