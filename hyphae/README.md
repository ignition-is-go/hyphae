# hyphae

A high-performance concurrent reactive programming library for Rust.

## Features

- **Fast reads** - Atomic value snapshots via `arc-swap`
- **Type-safe** - Compile-time checking with heterogeneous cell combinations
- **Thread-safe** - Safe concurrent access across threads
- **Dependency tracking** - Inspect and visualize cell relationships

## Quick Start

```rust
use hyphae::{
    Cell, JoinExt, MapExt, Materialize, Mutable, Signal, Watchable, flat,
};

let x = Cell::new(5);
let y = Cell::new(10);

// join stays lazy, then creates its required fan-in coalescing boundary
// when the chain is materialized.
let sum = x.clone().join(y).map(flat!(|a, b| a + b)).materialize();

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

All pipeline surfaces use `Materialize<T, S>`. The seedness parameter selects
the result shape: `Definite` produces `Cell<T>`, while `Empty` produces
`Cell<Option<T>>` for operators such as `filter` that may suppress their
initial emission. Import `Materialize` to call `.materialize()`.

Operators that need state or multiple sources (`debounce`, `buffer_*`, `join`,
`merge`, `switch_map`, and others) are lazy pipelines too. Their state and any
required fan-in boundary are created only when the pipeline is installed.
Call `.materialize()` at the point where you need a cached value, `get()`, or
`subscribe()`.

Pipeline values are deliberately not `Clone`. To retain a source after an
operator consumes it, clone the source handle before building the pipeline. To
share one pipeline installation across several consumers, use `.share()`, or
materialize once and clone the resulting `Cell`.

Upgrading from Hyphae 2.x? See
[Migrating to Hyphae 3.0](https://github.com/ignition-is-go/hyphae/blob/main/docs/migrating-to-v3.md).

## Operators

Transform, combine, and filter reactive streams. Pure operators below
(`map`, `filter`, `catch_error`) return pipelines — call `.materialize()`
when you need a cell. Stateful operators such as `scan`, `debounce`, and
`throttle` are lazy pipelines too.

```rust
use std::time::Duration;
use hyphae::{
    CatchErrorExt, DebounceExt, FilterExt, MapExt, Materialize, ScanExt, ThrottleExt,
};

let doubled = x.map(|v| v * 2).materialize();
let filtered = x.filter(|v| *v > 10).materialize();
let running_sum = numbers.scan(0, |acc, x| acc + x).materialize();
let debounced = input.debounce(Duration::from_millis(100)).materialize();
let throttled = input.throttle(Duration::from_millis(50)).materialize();
let safe = fallible.catch_error(|_| default).materialize();
```

## Reactive Collections

```rust
use hyphae::{CellMap, Gettable, Materialize};

let users = CellMap::<String, User>::new();
let admin = users.get(&"admin".to_string()).materialize();

users.insert("admin".to_string(), User::new());
assert!(admin.get().is_some()); // updates automatically
```

Reactive collection views (`get`, `entries`, `items`, `keys`, `size`, `len`,
and `diffs`) expose pipelines too. Some currently reuse an internal cached cell,
so their terminal `.materialize()` is a no-op today; keeping the boundary in the
public contract lets those implementations become fully deferred later without
another API break.

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

## Profiling

The `profiling` feature is hyphae's single observability switch. It costs
nothing per cell and ~1% on the hot path:

- `Cell::notify` / `write_value` / `fanout` become `#[inline(never)]`, so
  sampling profilers resolve them as distinct frames instead of one folded
  symbol.
- Each fanout emits a `tracing` span (`hyphae.fanout`) tagged with the cell's
  `id` and `name` (set names with `Cell::with_name`). hyphae only emits spans;
  the application attaches the subscriber (`tracing-flame`, `tracing-tracy`, …).
- `hyphae::profiling::pass` / `take_report` tally per-cell re-fires inside a
  measured propagation pass.

For live-cell counts and memory attribution, use a heap profiler
(jemalloc/`jeprof`, or `pprof`) rather than an in-process registry. See
[`docs/profiling.md`](docs/profiling.md).

## License

MIT OR Apache-2.0
