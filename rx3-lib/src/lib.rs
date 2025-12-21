//! # rx3 - Lock-Free Reactive Programming Library
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
//! use rx3::{Cell, MapExt, Mutable, SubscribeExt, JoinExt, flat};
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
//! let _guard = sum.subscribe(|value| {
//!     println!("Sum changed to: {}", value);
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
//! use rx3::{Cell, Gettable, JoinExt, MapExt, flat};
//!
//! let name = Cell::new("World".to_string());
//! let count = Cell::new(3);
//!
//! // flat! turns |n, c| into |(n, c)| automatically
//! let message = name.join(&count).map(flat!(|n, c| {
//!     format!("Hello, {}! (x{})", n, c)
//! }));
//!
//! assert_eq!(message.get(), "Hello, World! (x3)");
//!
//! // For 3+ cells, flat! handles the nesting
//! let a = Cell::new(1);
//! let b = Cell::new(2);
//! let c = Cell::new(3);
//!
//! // Without flat!: |((a, b), c)| - nested tuples
//! // With flat!: |a, b, c| - clean params
//! let sum = a.join(&b).join(&c).map(flat!(|x, y, z| x + y + z));
//! assert_eq!(sum.get(), 6);
//! ```

#[macro_use]
pub mod flat;
pub mod cell;
pub mod constructors;
pub mod subscription;
pub mod traits;

#[cfg(test)]
mod tests;

pub use cell::{Cell, CellImmutable, CellMutable};
pub use constructors::{from_iter_with_delay, interval};
pub use subscription::SubscriptionGuard;
pub use traits::{
    CatchErrorExt, DebounceExt, DedupedExt, DelayExt, DepNode, FilterExt, Gettable, JoinExt,
    MapErrExt, MapExt, MapOkExt, MergeMapExt, Mutable, PairwiseExt, ParallelCell, ParallelExt,
    ScanExt, SkipExt, SubscribeExt, SwitchMapExt, TakeExt, TapExt, ThrottleExt, TryMapExt,
    UnwrapOrExt,
};
