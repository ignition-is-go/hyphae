//! # hypha - Lock-Free Reactive Programming Library
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
//! use hypha::{Cell, MapExt, Mutable, Watchable, JoinExt, Signal, flat};
//!
//! // Create reactive cells
//! let x = Cell::new(5).with_name("x");
//! let y = Cell::new(10).with_name("y");
//!
//! // Derive new cells
//! let doubled = x.map(|val| val * 2).with_name("doubled");
//!
//! // Combine multiple cells with join + flat!
//! let sum = x.join(&y).map(flat!(|a, b| a + b)).with_name("sum");
//!
//! // Subscribe to changes (guard auto-unsubscribes on drop)
//! let _guard = sum.subscribe(|signal| {
//!     if let Signal::Value(value) = signal {
//!         println!("Sum changed to: {}", value);
//!     }
//! });
//!
//! // Updates propagate automatically
//! x.set(20); // Triggers updates to doubled and sum
//! ```
//!
//! ## Combining Cells
//!
//! Use `join()` to combine cells, and the `flat!` macro to avoid nested tuple destructuring:
//!
//! ```rust
//! use hypha::{Cell, Gettable, JoinExt, MapExt, flat};
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
pub mod metrics;
pub mod nested_map;
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
pub use nested_map::NestedMap;
pub use signal::Signal;
pub use subscription::SubscriptionGuard;
#[cfg(feature = "trace")]
pub use tracing::{CellTraceSnapshot, hot_cells as hot_traced_cells, log_hot_cells};
pub use traits::{
    AuditExt, BackpressureExt, BufferCountExt, BufferTimeExt, CatchErrorExt, CellValue, ColdExt,
    ConcatExt, CountByExt, DebounceExt, DedupedExt, DelayExt, DepNode, DistinctExt,
    DistinctUntilChangedByExt, FilterExt, FinalizeExt, FirstExt, Gettable, GroupByExt,
    HasForeignKey, IdFor, IdType, InnerJoinExt, JoinExt, JoinKeyFrom, KeyChange, LastExt,
    LeftJoinExt, LeftSemiJoinExt, MapErrExt, MapExt, MapOkExt, MergeExt, MergeMapExt, Mutable,
    PairwiseExt, ProjectCellExt, ProjectManyExt, ProjectMapExt, ReactiveKeys, ReactiveMap,
    RetryExt, SampleExt, ScanExt, SelectCellExt, SelectExt, SkipExt, SkipWhileExt,
    StateMachineBuilder, StateTransitionExt, SwitchMapExt, TakeExt, TakeUntilExt, TakeWhileExt,
    TapExt, ThrottleExt, TimeoutExt, TryMapExt, UnwrapOrExt, Watchable, WindowExt,
    WithLatestFromExt, ZipExt, join_vec,
};
#[cfg(not(target_arch = "wasm32"))]
pub use traits::{ParallelCell, ParallelExt};
