# hyphae

A lock-free reactive programming library for Rust.

## Features

- **Lock-free** - Atomic updates via `arc-swap`, no blocking
- **Type-safe** - Compile-time checking with heterogeneous cell combinations
- **Thread-safe** - Safe concurrent access across threads
- **Dependency tracking** - Inspect and visualize cell relationships

## Quick Start

```rust
use hyphae::{Cell, Signal, flat, JoinExt, MapExt, Mutable, Watchable};

let x = Cell::new(5);
let y = Cell::new(10);

// Combine cells with join + flat! macro
let sum = x.join(&y).map(flat!(|a, b| a + b));

// Subscribe to changes
let _guard = sum.subscribe(|signal| {
    if let Signal::Value(v) = signal {
        println!("Sum: {}", v);
    }
});

x.set(20); // prints "Sum: 30"
```

## Operators

Transform, combine, and filter reactive streams:

```rust
use std::time::Duration;
use hyphae::{MapExt, FilterExt, ScanExt, DebounceExt, ThrottleExt, CatchErrorExt};

let doubled = x.map(|v| v * 2);
let filtered = x.filter(|v| *v > 10);
let running_sum = numbers.scan(0, |acc, x| acc + x);
let debounced = input.debounce(Duration::from_millis(100));
let throttled = input.throttle(Duration::from_millis(50));
let safe = fallible.catch_error(|_| Cell::new(default));
```

## Reactive Collections

```rust
use hyphae::{CellMap, Gettable};

let users = CellMap::<String, User>::new();
let admin = users.get(&"admin".to_string()); // reactive cell

users.insert("admin".to_string(), User::new());
assert!(admin.get().is_some()); // updates automatically
```

## Async Support

```rust
use hyphae::{Cell, AsyncWatchableExt};

let cell = Cell::new(0);
let mut stream = cell.to_stream();

while let Some(value) = stream.next().await {
    println!("Got: {}", value);
}
```

Requires the `async` feature flag.

## Performance Monitoring

```rust
use std::time::Duration;
use hyphae::Cell;

let cell = Cell::with_metrics(0);

cell.on_slow_subscriber(Duration::from_millis(10), |alert| {
    eprintln!("Slow subscriber: {}ns", alert.duration_ns);
});
```

## License

MIT OR Apache-2.0