//! # hyphae - Lock-Free Reactive Programming Library
//!
//! A high-performance, type-safe reactive programming library featuring true lock-free operations,
//! heterogeneous cell combinations, and comprehensive dependency tracking.
//!
//! ## Features
//!
//! - **Lock-Free**: Uses `arc-swap` for atomic value updates without blocking
//! - **Type-Safe**: Full compile-time type checking with heterogeneous cell support
//! - **Automatic Propagation**: Changes flow through dependency chains automatically
//! - **Dependency Tracking**: Inspect and visualize cell relationships
//! - **Thread-Safe**: Safe concurrent access across threads
//!
//! ## Quick Start
//!
//! ```rust
//! use hyphae::{Cell, MapExt, Mutable, Watchable, JoinExt, Pipeline, Signal, flat};
//!
//! // Create reactive cells
//! let x = Cell::new(5).with_name("x");
//! let y = Cell::new(10).with_name("y");
//!
//! // Pure operators (map/filter/...) return pipelines — no allocation
//! // until you materialize.
//! let doubled = x.clone().map(|val| val * 2).materialize().with_name("doubled");
//!
//! // Combine multiple cells with join + flat!. join is stateful — it
//! // returns a Cell directly. Chaining .map fuses into the join's
//! // installed callback when materialized.
//! let sum = x.join(&y).map(flat!(|a, b| a + b)).materialize().with_name("sum");
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
//! consumer calls [`Pipeline::materialize`].
//!
//! [`Cell`] is the materialized, cached, multicast form. Subscribing requires
//! a cell — there is no `Pipeline::subscribe` by design, forcing callers to
//! make the memoization decision explicit.
//!
//! Stateful operators (`scan`, `debounce`, `throttle`, `buffer_*`, `pairwise`,
//! `window`, `distinct*`, `sample`, `delay`, `take`, `first`, `last`, `merge`,
//! `merge_map`, `switch_map`, `with_latest_from`, `zip`, `join`) return cells
//! directly — they hold per-subscription state, so memoization is unavoidable.
//!
//! ## Combining Cells
//!
//! Use `join()` to combine cells, and the `flat!` macro to avoid nested tuple destructuring.
//! `join` is stateful and returns a [`Cell`] directly, so the chain below is a cell
//! once `.map(...)` fuses onto it — no `.materialize()` needed for `.get()`:
//!
//! ```rust
//! use hyphae::{Cell, Gettable, JoinExt, MapExt, flat};
//!
//! let a = Cell::new(1);
//! let b = Cell::new(2);
//! let c = Cell::new(3);
//! let d = Cell::new(4);
//!
//! // Without flat!: |(((a, b), c), d)| - deeply nested
//! // With flat!: |a, b, c, d| - clean and simple
//! let sum = a.join(&b).join(&c).join(&d).map(flat!(|a, b, c, d| a + b + c + d));
//! assert_eq!(sum.get(), 10);
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
pub mod constructors;
pub mod map_query;
pub mod metrics;
pub mod nested_map;
pub mod pipeline;
pub mod signal;
pub mod subscription;
#[cfg(feature = "trace")]
pub mod tracing;
pub mod traits;

#[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
pub mod registry;
#[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
pub mod server;

#[cfg(test)]
mod tests;

#[cfg(feature = "async")]
pub use async_support::{AsyncWatchableExt, CellStream};
pub use bounded_input::{BoundedInput, BoundedInputMetrics, OverflowPolicy};
pub use bounded_output::BoundedOutput;
pub use cell::{Cell, CellImmutable, CellMutable, SlowSubscriberAlert};
pub use cell_map::{CellMap, MapDiff, WeakCellMap};
pub use cell_set::{CellSet, SetDiff};
pub use constructors::from_iter_with_delay;
#[cfg(not(target_arch = "wasm32"))]
pub use constructors::{IntervalTick, interval, interval_precise, interval_precise_with_elapsed};
pub use metrics::CellMetrics;
pub use map_query::MapQuery;
pub use nested_map::NestedMap;
pub use pipeline::Pipeline;
pub use signal::Signal;
pub use subscription::SubscriptionGuard;
#[cfg(feature = "trace")]
pub use tracing::{CellTraceSnapshot, hot_cells as hot_traced_cells, log_hot_cells};
pub use traits::{
    AuditExt, BackpressureExt, BufferCountExt, BufferTimeExt, CatchErrorExt, CellValue, ColdExt,
    ConcatExt, CountByExt, CountByPlan, DebounceExt, DedupedExt, DelayExt, DepNode, DistinctExt,
    DistinctUntilChangedByExt, FilterExt, FilterPipeline, FinalizeExt, FirstExt, Gettable,
    GroupByExt, GroupByPlan, HasForeignKey, IdFor, IdType, InnerJoinByKeyPlan, InnerJoinByPairPlan,
    InnerJoinExt, JoinExt, JoinKeyFrom, KeyChange,
    LastExt, LeftJoinExt, LeftJoinPlan, LeftSemiJoinExt, LeftSemiJoinPlan, MapErrExt, MapExt,
    MapOkExt, MapPipeline, MergeExt, MergeMapExt,
    MultiLeftJoinExt, MultiLeftJoinPlan, Mutable, PairwiseExt, ProjectCellExt, ProjectCellPlan,
    ProjectManyExt, ProjectManyPlan, ProjectMapExt, ProjectPlan,
    ReactiveKeys, ReactiveMap, RetryExt, SampleExt, ScanExt, SelectCellExt, SelectCellPlan,
    SelectExt, SelectPlan, SkipExt, SkipWhileExt, StateMachineBuilder, StateTransitionExt,
    SwitchMapExt, TakeExt, TakeUntilExt, TakeWhileExt, TapExt, TapPipeline, ThrottleExt,
    TimeoutExt, TryMapExt, TryMapPipeline, UnwrapOrExt, Watchable, WatchableResult, WindowExt,
    WithLatestFromExt, ZipExt, join_vec,
};
#[cfg(not(target_arch = "wasm32"))]
pub use traits::{ParallelCell, ParallelExt};
