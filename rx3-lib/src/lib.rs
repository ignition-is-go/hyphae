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
//! use rx3::{Cell, combine, traits::{Mutable, Watchable}};
//!
//! // Create reactive cells
//! let x = Cell::new(5).with_name("x");
//! let y = Cell::new(10).with_name("y");
//!
//! // Derive new cells
//! let doubled = x.map(|val| val * 2).with_name("doubled");
//!
//! // Combine multiple cells
//! let sum = combine!((&x, &y), |a: &i32, b: &i32| {
//!     a + b
//! }).with_name("sum");
//!
//! // Watch for changes
//! sum.watch(|value| {
//!     println!("Sum changed to: {}", value);
//! });
//!
//! // Updates propagate automatically
//! x.set(20); // Triggers updates to doubled and sum
//! ```
//!
//! ## Dependency Tracking
//!
//! Every cell tracks its parent dependencies, enabling introspection and debugging:
//!
//! ```rust
//! use rx3::{Cell, combine, traits::{Mutable, Watchable}};
//!
//! let x = Cell::new(5).with_name("x");
//! let y = Cell::new(10).with_name("y");
//! let sum = combine!((&x, &y), |a: &i32, b: &i32| { a + b }).with_name("sum");
//!
//! // Inspect dependencies
//! assert_eq!(sum.dependency_count(), 2);
//! assert!(sum.has_dependencies());
//!
//! // Visualize dependency structure
//! sum.print_dependencies();
//! // Output:
//! // sum
//! //   depends on:
//! //     - x
//! //     - y
//!
//! // Get dependency metadata
//! for (id, name) in sum.dependencies() {
//!     println!("Depends on: {:?}", name);
//! }
//! ```
//!
//! ## Combining Cells
//!
//! The `combine!` macro supports multiple cells of different types:
//!
//! ```rust
//! use rx3::{Cell, combine, traits::{Mutable, Watchable}};
//!
//! let name = Cell::new("World".to_string());
//! let count = Cell::new(3);
//!
//! let message = combine!((&name, &count), |n: &String, c: &i32| {
//!     format!("Hello, {}! (x{})", n, c)
//! });
//!
//! assert_eq!(message.get(), "Hello, World! (x3)");
//! ```
//!
//! ## Examples
//!
//! See the `examples/` directory for complete demonstrations:
//! - `dependency_tracking.rs` - Basic dependency tracking
//! - `advanced_dependencies.rs` - Complex dependency graphs

#[macro_use]
pub mod combine;
pub mod cell;
pub mod traits;

pub use cell::{Cell, CellImmutable, CellMutable};
pub use traits::{DedupedExt, DepNode, JoinExt, MapExt, Mutable, Watchable};
