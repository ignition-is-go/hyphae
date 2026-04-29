# hyphae

A lock-free reactive programming library for Rust.

## Features

- **Lock-free** - Atomic updates via `arc-swap`, no blocking
- **Type-safe** - Compile-time checking with heterogeneous cell combinations
- **Thread-safe** - Safe concurrent access across threads
- **Dependency tracking** - Inspect and visualize cell relationships

## Quick Start

```rust
use hyphae::{Cell, Signal, flat, JoinExt, MapExt, Mutable, Pipeline, Watchable};

let x = Cell::new(5);
let y = Cell::new(10);

// join is stateful and returns a Cell. Chaining .map fuses into the
// join's installed callback when materialized; .materialize() compiles
// the chain into a cell you can subscribe to.
let sum = x.join(&y).map(flat!(|a, b| a + b)).materialize();

// Subscribe to changes
let _guard = sum.subscribe(|signal| {
    if let Signal::Value(v) = signal {
        println!("Sum: {}", v);
    }
});

x.set(20); // prints "Sum: 30"
```

## Pipelines vs Cells

Pure operators (`map`, `filter`, `try_map`, `tap`, `map_ok`, `map_err`,
`catch_error`, `unwrap_or`) return a `Pipeline` — an uncompiled chain that
fuses closures at compile time. Call `.materialize()` to compile the chain
into a `Cell` you can subscribe to. There is no `Pipeline::subscribe` by
design: callers make the memoization decision explicit.

Stateful operators (`scan`, `debounce`, `throttle`, `buffer_*`, `pairwise`,
`window`, `distinct*`, `sample`, `delay`, `take`, `first`, `last`, `merge`,
`merge_map`, `switch_map`, `with_latest_from`, `zip`, `join`) return cells
directly — they hold per-subscription state, so memoization is unavoidable.

## Operators

Transform, combine, and filter reactive streams. Pure operators below
(`map`, `filter`, `catch_error`) return pipelines — call `.materialize()`
when you need a cell. Stateful operators (`scan`, `debounce`, `throttle`)
return cells directly.

```rust
use std::time::Duration;
use hyphae::{MapExt, FilterExt, ScanExt, DebounceExt, ThrottleExt, CatchErrorExt, Pipeline};

let doubled = x.map(|v| v * 2).materialize();
let filtered = x.filter(|v| *v > 10).materialize();
let running_sum = numbers.scan(0, |acc, x| acc + x);
let debounced = input.debounce(Duration::from_millis(100));
let throttled = input.throttle(Duration::from_millis(50));
let safe = fallible.catch_error(|_| Cell::new(default)).materialize();
```

## Reactive Collections

```rust
use hyphae::{CellMap, Gettable};

let users = CellMap::<String, User>::new();
let admin = users.get(&"admin".to_string()); // reactive cell

users.insert("admin".to_string(), User::new());
assert!(admin.get().is_some()); // updates automatically
```

## Map Queries vs CellMaps

Pure `CellMap` operators (`inner_join`, `left_join`, `project`, `select`,
`count_by`, `group_by`, ...) return uncompiled `MapQuery` plan nodes — not
`CellMap`s. Plans compose: any plan or `CellMap` can feed any other
operator's input. Call `.materialize()` to allocate ONE output `CellMap`
with ONE subscription per root source, replacing what would otherwise be N
intermediate maps for an N-stage chain.

`MapQuery` plan nodes are not `Clone` (mirroring `Pipeline`). To share work
across consumers, materialize once and clone the resulting `CellMap`.

```rust
use hyphae::{CellMap, MapQuery, traits::{InnerJoinExt, ProjectMapExt}};

let users = CellMap::<String, &'static str>::new();
let scores = CellMap::<String, i32>::new();
users.insert("u1".into(), "alice");
scores.insert("u1".into(), 42);

// Chained operators return plan nodes — no intermediate CellMap
// until materialize().
let view = users
    .clone()
    .inner_join(scores.clone())
    .project(|user_id, (name, score)| Some((user_id.clone(), format!("{name}:{score}"))))
    .materialize();

assert!(view.contains_key(&"u1".to_string()));
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