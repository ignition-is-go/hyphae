# Migrating to Hyphae 3.0

Hyphae 3.0 moves the remaining reactive operators from eagerly created output
cells to lazy pipelines. The computation, its subscriptions, and its mutable
operator state are installed only when a caller chooses a materialization
boundary.

This is a source-breaking change. Most migrations are mechanical, but the new
boundary is also an opportunity to avoid intermediate caches: build the entire
operator chain first and call `.materialize()` once at the point where the
application needs an observable value.

## Import the materialization trait

`materialize` is provided by one capability trait for every pipeline shape:

```rust
use hyphae::Materialize;
```

- `Materialize<T, Definite>` produces `Cell<T>` for pipelines with a definite
  seed.
- `Materialize<T, Empty>` produces `Cell<Option<T>>` for pipelines that may
  suppress their initial emission, notably `filter`.

Import `Materialize` in every module that calls `.materialize()`. `Pipeline`
itself deliberately does not provide `get` or `subscribe` methods.

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

Keep the terminal materialized cell alive for as long as the application needs
the reactive graph to update. The cell owns the installed subscriptions;
dropping it tears the graph down. A temporary read such as
`build_report().materialize().get()` observes the current seed and then removes
the installation at the end of the statement. It will not remain subscribed
across a later source update. Store the terminal cell in the owning component
or topology instead:

```rust
struct ViewModel {
    output: Cell<Rendered, CellImmutable>,
}

let view_model = ViewModel {
    output: source.clone().map(render).materialize(),
};
```

One retained terminal boundary keeps the complete upstream pipeline alive;
intermediate materializations are not required solely for lifetime.

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
`Cell<T>`. `filter` therefore has `Empty` seedness and materializes to
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

Generic helpers can accept or return `impl Materialize<T, S>` when the
seedness is generic. Public operator factories intentionally hide their
concrete plan nodes behind this contract so implementations can fuse or change
storage strategies in compatible 3.x releases.

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

## `CellMap` query plans

`MapQuery` uses associated key and value types and retains an explicit,
consuming materialization boundary. Compose the complete query and materialize
once:

```rust
use hyphae::{MapQuery, traits::{InnerJoinExt, MapValuesExt}};

let view = users
    .clone()
    .inner_join(scores.clone())
    .map_values(|_user_id, (user, score)| build_row(user, score))
    .materialize();
```

Use `MapQueryShareExt::share` for an explicit shared lazy boundary, or clone the
materialized `CellMap`. Ordinary plans intentionally are not `Clone`.

### Renamed and typed query APIs

| Before | Hyphae 3.0 |
| --- | --- |
| `MapQuery<K, V>` | `MapQuery<Key = K, Value = V>` |
| `project` / `ProjectMapExt` preserving keys | `map_values` or `filter_map_values` |
| `project` rekeying rows | `map_entries` or `filter_map_entries` |
| `project_many` | no fully mechanical replacement: return local keys to `flat_map_entries`, yielding `(source_key, local_key)`, or redesign global-key consumers |
| an implicit `HasForeignKey<Parent>` relationship | one named `ForeignKeyRelation` marker per semantic relationship |
| schema-generated `*_join_by` calls | `*_join_fk::<Relation, _>` when the schema owns the relationship |

`map_entries` and `filter_map_entries` create a semantic repartition boundary.
Their output keys must be globally unique across current source rows. A
collision is validated before output mutation and panics synchronously; it is
never resolved by last-writer-wins. `flat_map_entries` returns output keys
`(source_key, local_key)`, which prevents collisions between different source
rows. A row must still emit each `local_key` at most once. Old `project_many`
closures that emitted caller-chosen global keys require a downstream key-type
migration to `(SourceKey, LocalKey)`. If global 1:N identity was semantically
required, redesign around scoped keys or an explicit grouping/aggregation and
materialization boundary; there is no mechanical last-writer-wins replacement.

A `ForeignKeyRelation` marker is the relationship's semantic, partition, and
index identity. Its extractor returns `Some(key)` for a present relationship
and `None` for an absent optional relationship; absent right rows do not enter
the relation index. `inner_join_fk` and `left_join_fk` join through
`IdFor<Relation::Parent>::MapKey`, independently of the current left payload.
Use the `*_join_by` variants as ad hoc extractor escape hatches when there is no
schema-owned relationship identity. Schema owners or generators should emit
the marker structs and their implementations. Hyphae contains no schema-code
generator; annotation syntax is an upstream generator decision.

For example, schema-owned code can name required and optional relationships to
the same parent independently:

```rust
use hyphae::traits::{ForeignKeyRelation, IdFor, LeftJoinExt};

struct User;
struct PostAuthor;
struct PostEditor;

impl IdFor<User> for UserId {
    type MapKey = UserId;
    fn map_key(&self) -> UserId { self.clone() }
}

impl ForeignKeyRelation for PostAuthor {
    type Parent = User;
    type Child = Post;
    type ForeignKey = UserId;
    fn foreign_key(post: &Post) -> Option<UserId> {
        Some(post.author_id.clone()) // required by this schema
    }
}

impl ForeignKeyRelation for PostEditor {
    type Parent = User;
    type Child = Post;
    type ForeignKey = UserId;
    fn foreign_key(post: &Post) -> Option<UserId> {
        post.editor_id.clone() // optional: None is absent
    }
}

let authored = users.clone().left_join_fk::<PostAuthor, _>(posts.clone());
let edited = users.left_join_fk::<PostEditor, _>(posts);
```

Recognized fluent left-join/projection chains retain specialized one- and
two-join forms. The third recognized join promotes the chain to a concrete,
statically typed `JoinRegion`; later recognized joins extend that typed region
to arbitrary depth. A rekey or an unsupported algebra shape is a physical
region boundary, not a promise of universal fusion.

`NestedMap` remains the separate public, read-only grouped view created by
`CellMap::nest`. It implements `ReactiveMap` and can be a `MapQuery` source. It
has not been renamed or unified with compiler-private relationship indexes.

Query closures must be deterministic, externally side-effect-free, and
nonblocking. The runtime may repeat or concurrently invoke them; invocation
count, order, and thread are not stable contracts. Observable output diffs are
still deterministic and ordered, and source mutation is synchronously settled
before returning. `ReactiveMap` carries `MapDiff` and has no completion/error
terminal channel, so completion/error ordering is currently not applicable to
map queries.

On native builds with `scheduler`, only sufficiently costly and balanced join
regions use the shared dedicated Rayon pool. Builds without `scheduler` and
wasm stay sequential. `HYPHAE_WORKER_THREADS` controls the pool and zero
disables it; `HYPHAE_WAVE_THREADS` is the compatibility fallback.

A panic inside a coordinated promoted `JoinRegion` joins sibling workers,
publishes no changes for that failed region application, clears queued or
reentrant region work, and poisons every physical root in that materialized
query cohort. Later root callbacks panic rather than continue from partial
region state. This is terminal fail-stop, not rollback: it does not undo the
source mutation, earlier output/subscriber work already published before the
callback, subscriber side effects, or ordinary callbacks outside a promoted
region.

## Migration checklist

1. Import `Materialize` wherever `.materialize()` is called.
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
9. Retain each terminal materialized cell for the full lifetime in which its
   source updates must propagate.
10. Change generic query bounds to `MapQuery<Key = K, Value = V>`.
11. Replace `project` with the semantic projection whose cardinality and key
    behavior match the old closure.
12. Migrate `project_many` local identity and downstream key types to
    `(SourceKey, LocalKey)`, or redesign callers that require global 1:N keys.
13. Audit every rekeying closure for the new unique-output-key contract.
14. Replace implicit foreign-key traits with named `ForeignKeyRelation`
    markers and use typed `*_join_fk` calls where appropriate.
15. Keep query closures pure and do not depend on their invocation order,
    count, or worker thread.
16. Run the application test suite with the same Hyphae features used in
    production, especially `scheduler`, `async`, and `profiling`; also test the
    sequential configuration when production may run on wasm or without the
    scheduler feature.
