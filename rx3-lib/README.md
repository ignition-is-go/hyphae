# rx3

A lock-free reactive programming library for Rust with dependency tracking.

## Features

- 🔒 **Lock-Free** - True lock-free operations using `arc-swap`
- 🎯 **Type-Safe** - Full compile-time type checking
- 🔗 **Heterogeneous Cells** - Combine cells of different types
- 📊 **Dependency Tracking** - Inspect and visualize cell relationships
- 🧵 **Thread-Safe** - Safe concurrent access without blocking

## Quick Start

```rust
use rx3::{Cell, combine, traits::{Mutable, Watchable}};

// Create reactive cells
let x = Cell::new(5).with_name("x");
let y = Cell::new(10).with_name("y");

// Derive new cells
let doubled = x.map(|val| val * 2);

// Combine multiple cells
let sum = combine!((&x, &y), |a: &i32, b: &i32| { a + b });

// Watch for changes
sum.watch(|value| {
    println!("Sum: {}", value);
});

// Updates propagate automatically
x.set(20);
```

## Dependency Tracking

```rust
let x = Cell::new(5).with_name("x");
let y = Cell::new(10).with_name("y");
let sum = combine!((&x, &y), |a: &i32, b: &i32| { a + b }).with_name("sum");

// Inspect dependencies
println!("Dependencies: {}", sum.dependency_count()); // 2

// Visualize structure
sum.print_dependencies();
// Output:
// sum
//   depends on:
//     - x
//     - y
```

## Examples

Run the included examples to see dependency tracking in action:

```bash
cargo run --example dependency_tracking
cargo run --example advanced_dependencies
```

## Documentation

Full API documentation is available via rustdoc:

```bash
cargo doc --open
```

## License

MIT OR Apache-2.0