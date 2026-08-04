# Migrating to Hyphae 3.0

Hyphae 3.0 moves the remaining reactive operators from eagerly created output
cells to lazy pipelines. The computation, its subscriptions, and its mutable
operator state are installed only when a caller chooses a materialization
boundary.

This is a source-breaking change. Most migrations are mechanical, but the new
boundary is also an opportunity to avoid intermediate caches: build the entire
operator chain first and call `.materialize()` once at the point where the
application needs an observable value.

## Import the materialization traits

`materialize` is provided by one of two traits, according to whether the
pipeline has a guaranteed initial value:

```rust
use hyphae::{MaterializeDefinite, MaterializeEmpty};
```

- `MaterializeDefinite` produces `Cell<T>` for pipelines with a definite seed.
- `MaterializeEmpty` produces `Cell<Option<T>>` for pipelines that may suppress
  their initial emission, notably `filter`.

Import the trait or traits used by a module. `Pipeline` itself deliberately
does not provide `get`, `subscribe`, or `materialize` methods.

## Add an explicit materialization boundary

Operators that previously returned a cell now return a pipeline. Add
`.materialize()` before the first operation that requires a cell, such as
`get`, `subscribe`, `with_name`, or an API bounded by `Watchable`.

Before:

```rust
let debounced = source.debounce(Duration::from_millis(100));
let _guard = debounced.subscribe(handle_value);
```

After:

```rust
let debounced = source
    .clone()
    .debounce(Duration::from_millis(100))
    .materialize();
let _guard = debounced.subscribe(handle_value);
```

Keep chains lazy until their leaf:

```rust
let output = source
    .clone()
    .debounce(Duration::from_millis(100))
    .map(normalize)
    .take(20)
    .materialize();
```

Materializing between these operators would create an extra cached multicast
cell and an extra propagation boundary. Do that only when the intermediate
value is intentionally observed or shared.

The operators moved to lazy pipelines in this release include `audit`,
`buffer_count`, `buffer_time`, `concat`, `debounce`, `delay`, `drop_newest`,
`drop_oldest`, `join`, `join_vec`, `merge`, `merge_map`, `retry`, `retry_when`,
`sample_latest`, `state_transition`, `switch_map`, `take_until`, `throttle`,
`timeout`, `window`, and `zip`.

Reactive collection views now follow the same rule. `CellMap::get`, `entries`,
`items`, `keys`, `size`, `len`, and `diffs`, plus `CellSet::contains`, `values`,
`len`, and `diffs`, return definite pipeline surfaces. Add `.materialize()`
before reading or subscribing:

```rust
let selected = users
    .get(&user_id)
    .map(render_user)
    .materialize();

let count = users.size().materialize();
```

Some of these views currently reuse or construct an internal cell, so the
terminal materialization has no additional runtime cost. The explicit boundary
is nevertheless part of the v3 API and leaves their caching strategy free to
change in later compatible releases. `Source::sample_on` follows the same
contract. Explicit conversion and source APIs such as `to_cell`, `lock`, and
`interval` continue to return materialized values directly.

## Receivers and additional inputs are owned

Pipeline operators consume `self`. Multi-source operators also take their
other pipeline inputs by value instead of borrowing a cell. Clone a source
handle before passing it into a pipeline when the application still needs that
handle afterward. Cloning a `Cell` is an inexpensive shared-handle clone; it
does not duplicate the stored value or subscriber registry.

Before:

```rust
let joined = left.join(&right);
left.set(next_left);
right.set(next_right);
```

After:

```rust
let joined = left
    .clone()
    .join(right.clone())
    .materialize();
left.set(next_left);
right.set(next_right);
```

The same owned-input pattern applies to `concat`, `merge`, `take_until`, and
`zip`. `join_vec` still owns its vector, but now returns a pipeline that must be
materialized before observation.

## Pipelines are not `Clone`

An ordinary pipeline represents one installation recipe and is deliberately
not `Clone`: silently cloning it would duplicate subscriptions and operator
work. Choose the sharing behavior explicitly.

To cache once and multicast from a cell:

```rust
let cached = source.clone().map(expensive).materialize();
let consumer_a = cached.clone();
let consumer_b = cached.clone();
```

To keep downstream branches lazy while sharing one upstream installation, use
`PipelineShareExt::share`:

```rust
use hyphae::PipelineShareExt;

let shared = source.clone().map(expensive).share();
let consumer_a = shared.clone().map(branch_a).materialize();
let consumer_b = shared.clone().map(branch_b).materialize();
```

Each terminal `.materialize()` normally installs its own pipeline state. A
share point makes that upstream installation explicit and reference-counted.

## Empty pipelines materialize to `Cell<Option<T>>`

A pipeline that can suppress its initial emission cannot honestly seed a
`Cell<T>`. `filter` therefore uses `MaterializeEmpty` and produces
`Cell<Option<T>>`:

```rust
let evens = source.clone().filter(|value| value % 2 == 0).materialize();

match evens.get() {
    Some(value) => use_even(value),
    None => waiting_for_first_match(),
}
```

The cell starts as `None` when the initial value does not pass. After the first
matching emission it becomes `Some(value)`. Later rejected emissions do not
reset it to `None`; they simply produce no update.

Generic helpers may need to express seedness and materialization as separate
bounds. Prefer accepting a concrete source or returning `impl Pipeline<...>`
unless callers genuinely need to abstract over both definite and empty
pipelines.

## Dynamic operators accept lazy inner pipelines

`switch_map` and `merge_map` accept inner pipeline recipes directly. Do not
materialize inside their mapper merely to satisfy the old cell-returning
signature. Keeping the inner chain lazy lets it fuse into the dynamic
installation boundary:

```rust
let selected = ids
    .clone()
    .switch_map(move |id| records.get(id).map(transform_record))
    .materialize();
```

Materialize an inner recipe only when that inner value needs its own cache or
must be shared independently of the dynamic operator.

## CellMap query plans

`MapQuery` retains the explicit boundary introduced in Hyphae 2.x. Continue to
compose joins, projections, and selections as a plan and materialize once:

```rust
let view = users
    .clone()
    .inner_join(scores.clone())
    .project(build_row)
    .materialize();
```

As with scalar pipelines, clone the resulting `CellMap` to share its cache, or
use `MapQueryShareExt::share` when downstream query branches should remain
lazy.

## Migration checklist

1. Import `MaterializeDefinite` and, where needed, `MaterializeEmpty`.
2. Follow compiler errors from `get`, `subscribe`, `with_name`, and
   `Watchable` bounds back to the intended cache boundary.
3. Add one terminal `.materialize()` after the complete operator chain.
4. Change borrowed secondary inputs such as `.join(&other)` to owned pipeline
   inputs such as `.join(other.clone())`.
5. Clone source handles before consuming operators when later code still
   mutates or reads those sources.
6. Replace attempts to clone a pipeline with either `.share()` or a single
   materialization followed by cheap `Cell` clones.
7. Update types downstream of empty pipelines to handle `Option<T>`.
8. Materialize reactive `CellMap`/`CellSet` views before calling `get`,
   `subscribe`, or passing them to a `Watchable` bound.
9. Run the application test suite with the same Hyphae features used in
   production, especially `scheduler`, `async`, and `profiling`.
