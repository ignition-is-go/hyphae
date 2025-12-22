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
pub mod signal;
pub mod subscription;
pub mod traits;

#[cfg(test)]
mod tests;

#[cfg(feature = "async")]
pub use async_support::{AsyncWatchableExt, CellStream};
pub use bounded_input::{BoundedInput, BoundedInputMetrics, OverflowPolicy};
pub use bounded_output::BoundedOutput;
pub use cell::{Cell, CellImmutable, CellMutable, SlowSubscriberAlert};
pub use cell_map::{CellMap, MapDiff};
pub use cell_set::{CellSet, SetDiff};
pub use constructors::{from_iter_with_delay, interval};
pub use metrics::CellMetrics;
pub use signal::Signal;
pub use subscription::SubscriptionGuard;
pub use traits::{
    AuditExt, BackpressureExt, BufferCountExt, BufferTimeExt, CatchErrorExt, ConcatExt,
    DebounceExt, DedupedExt, DelayExt, DepNode, DistinctExt, DistinctUntilChangedByExt, FilterExt,
    FinalizeExt, FirstExt, Gettable, JoinExt, LastExt, MapErrExt, MapExt, MapOkExt, MergeExt,
    MergeMapExt, Mutable, PairwiseExt, ParallelCell, ParallelExt, RetryExt, SampleExt, ScanExt,
    SkipExt, SkipWhileExt, StateMachineBuilder, StateTransitionExt, SwitchMapExt, TakeExt,
    TakeUntilExt, TakeWhileExt, TapExt, ThrottleExt, TimeoutExt, TryMapExt, UnwrapOrExt, Watchable,
    WindowExt, WithLatestFromExt, ZipExt,
};
