# Statically Compiled MapQuery Engine

**Date:** 2026-08-10
**Status:** Proposed v3 architecture; API decisions approved in principle, benchmark spike required before full implementation
**Tracking:** `lv-d884`
**Supersedes:** The Phase B optimizer direction in `2026-04-24-cell-map-query-plans-design.md`

## Decision

Hyphae's collection-query API will describe relational facts in types and
compile each materialized query into a fully monomorphized incremental program.
The default runtime favors update latency and efficient parallel work even when
that substantially increases rustc time, memory use, and generated code size.

Rayon is the execution backend, not the query architecture. A compiled plan
identifies coarse, partition-local regions; Rayon executes those regions over
exclusive shards. Externally visible publication remains synchronous,
deterministic, and ordered.

The intended flow is:

```text
strongly typed MapQuery expression
        |
        | materialize()
        v
monomorphized plan-specific runtime
        |
        +-- direct root entry points
        +-- typed relationship indexes
        +-- fused projection regions
        +-- reusable plan scratch
        +-- sequential/parallel cost decision
        v
partition-local execution on the dedicated Rayon pool
        |
        v
deterministic merge and one observable output commit
```

This is the collection analogue of scalar `Pipeline` fusion. Scalar fusion has
already demonstrated that compile-time specialization is worth an aggressive
compile-resource trade: a depth-500 chain improved from approximately 1.27 ms
to 12.5 us by collapsing intermediate reactive machinery into a statically
visible closure chain.

## Goals

1. Compile an arbitrary chain of selections, projections, and joins into one
   plan-specific runtime with no dynamic dispatch between hot stages.
2. Express cardinality, key preservation, relationship identity, and
   partitioning facts through operator and relationship types rather than
   opaque extractor closures.
3. Remove intermediate `MapDiff` allocation, per-stage `Arc`/`Weak` traffic,
   and per-stage synchronization from fused regions.
4. Reuse a relationship index when the same source/relationship pair is used
   more than once in one materialized plan.
5. Exploit multiple cores without weakening synchronous settlement or
   deterministic output ordering.
6. Keep one explicit observable boundary: `materialize()` returns a `CellMap`.
7. Make small updates remain fast by selecting the sequential path below a
   measured plan-specific parallel threshold.

## Non-goals

- A SQL parser, textual query language, or runtime query optimizer.
- Statistics-driven join reordering in the first implementation.
- Preserving the invocation order of side effects hidden inside user query
  closures. Query closures become explicitly pure by contract.
- Parallelizing every callback or placing a Rayon barrier after every logical
  operator.
- Improving scalar `Cell` pipelines; they retain their existing `Pipeline`
  compiler and scheduler integration.
- Guaranteeing that every possible query is faster in parallel. Parallelism is
  conditional and cost-gated.

## Current architecture and its remaining costs

The v3 `MapQuery` work already removed intermediate observable `CellMap`s. A
query plan is currently installed recursively, however, and every stage wraps
its downstream stage in an `Arc<dyn Fn(&MapDiff<...>)>` sink. Stateful stages
own independent `Mutex`-protected runtimes and communicate by constructing and
dispatching diffs.

For projection stages, the generic runtime maintains:

```text
source_rows
source_output_keys
output_cache
```

even when an operator is a key-preserving filter and its incoming diff already
contains the old and new values required to update the output. Join stages
likewise maintain private forward/reverse indexes even when another join in the
same query indexes the same source by the same relationship.

The measured residual collection cost is dominated by:

- a mutex acquisition per join stage;
- hash lookup and impacted-key bookkeeping per stage;
- boxed diff-sink dispatch per output diff;
- transient diff and work-set construction;
- duplicated source mirrors and relationship indexes.

The current type tree contains the information required to do better, but the
installer erases it before execution.

## Public relational algebra

### Associated key and value types

The breaking v3 window should replace `MapQuery<K, V>` with associated types.
This avoids repeating unconstrained key/value generic parameters throughout
relationship-typed APIs and allows one explicit relationship generic at a join
call site.

```rust
pub trait MapQuery: private::Sealed + Sized + Send + Sync + 'static {
    type Key: Hash + Eq + CellValue;
    type Value: CellValue;

    fn materialize(self) -> CellMap<Self::Key, Self::Value, CellImmutable>;
}
```

Operators return opaque plans with associated-type equality:

```rust
fn map_values<U>(
    self,
    f: impl Fn(&Self::Key, &Self::Value) -> U + Send + Sync + 'static,
) -> impl MapQuery<Key = Self::Key, Value = U>
where
    U: CellValue;
```

Plans remain consuming, non-`Clone`, non-subscribable values. `materialize()`
remains the only observation boundary.

### Projection vocabulary

The general `project`/`project_many` names hide properties the compiler must
otherwise guess. Replace them with operators whose types state cardinality and
key behavior:

| Operator | Cardinality | Output key | Partition effect |
| --- | ---: | --- | --- |
| `select` | 1:0/1 | unchanged | preserved |
| `select_by` | 1:0/1 | unchanged | preserved |
| `map_values` | 1:1 | unchanged | preserved |
| `filter_map_values` | 1:0/1 | unchanged | preserved |
| `map_entries` | 1:1 | may change | repartition if required |
| `filter_map_entries` | 1:0/1 | may change | repartition if required |
| `flat_map_entries` | 1:N | may change | repartition if required |

Representative signatures:

```rust
fn select(
    self,
    predicate: impl Fn(&Self::Value) -> bool + Send + Sync + 'static,
) -> impl MapQuery<Key = Self::Key, Value = Self::Value>;

fn select_by(
    self,
    predicate: impl Fn(&Self::Key, &Self::Value) -> bool + Send + Sync + 'static,
) -> impl MapQuery<Key = Self::Key, Value = Self::Value>;

fn filter_map_values<U>(
    self,
    f: impl Fn(&Self::Key, &Self::Value) -> Option<U> + Send + Sync + 'static,
) -> impl MapQuery<Key = Self::Key, Value = U>
where
    U: CellValue;

fn map_entries<K2, V2>(
    self,
    f: impl Fn(&Self::Key, &Self::Value) -> (K2, V2) + Send + Sync + 'static,
) -> impl MapQuery<Key = K2, Value = V2>
where
    K2: Hash + Eq + CellValue,
    V2: CellValue;
```

`map_values` and `filter_map_values` are the preferred projection operators.
They carry an exact proof that the map key and key partition remain unchanged.
Arbitrary rekeying remains supported, but it creates a physical-plan boundary
unless the following operator uses the new output-key partition directly.

Internally, operator plan types expose properties as associated marker types,
not boolean runtime flags:

```rust
pub(crate) trait PlanProperties {
    type Cardinality: Cardinality;
    type InputPartition: Partition;
    type OutputPartition: Partition;
}

pub(crate) struct ExactlyOne;
pub(crate) struct ZeroOrOne;
pub(crate) struct Many;

pub(crate) struct ByMapKey<K>(PhantomData<K>);
pub(crate) struct ByRelation<R>(PhantomData<R>);
pub(crate) struct Repartition<K>(PhantomData<K>);
```

These markers select runtime composition and partition routing at compile
time. They are not exposed as a user-extensible optimizer protocol; `MapQuery`
remains sealed so Hyphae controls the semantic guarantees attached to every
operator.

### Rekey collision semantics

Two source rows mapping to the same output key must not silently overwrite one
another. The implementation must preserve deterministic incremental behavior.
The API therefore distinguishes these cases:

- Key-preserving operators are collision-free by construction.
- Many-to-one transformations use an explicit grouping/aggregation operator.
- `map_entries` and `filter_map_entries` carry a documented unique-output-key
  contract. A collision is a query error, never last-writer-wins state.
- `flat_map_entries` identifies an output by `(source_key, local_key)` unless
  the caller explicitly requests a globally unique output-key form.

The error-delivery surface for a unique-key contract violation must be decided
before the rekeying operators are implemented. It may use Hyphae's existing
error signal path, but it must not be a debug-only assertion.

### Pure closure contract

All query predicates, projections, key calculations, and aggregators must be:

- deterministic for the same arguments;
- free of externally visible side effects;
- non-blocking;
- safe to invoke concurrently when the plan selects parallel execution.

Rust's `Fn + Send + Sync` bounds enforce concurrency safety, not purity. The
purity requirement is a documented semantic contract. Hyphae guarantees
deterministic query results and publication order; it does not guarantee the
relative invocation order of user closures within a parallel batch.

## Relationship-typed foreign keys

### Relationship identity

The existing `HasForeignKey<T>` identifies a foreign key only by its parent
entity type. Rust coherence permits only one such implementation for a given
child/parent pair, so it cannot represent both `Document.created_by -> User`
and `Document.updated_by -> User`.

Replace parent-only identity with a zero-sized relationship marker:

```rust
pub trait ForeignKeyRelation: Send + Sync + 'static {
    type Parent: Send + Sync + 'static;
    type Child: CellValue;
    type ForeignKey: IdFor<Self::Parent> + CellValue;

    fn foreign_key(child: &Self::Child) -> Option<Self::ForeignKey>;
}

pub struct DocumentCreator;

impl ForeignKeyRelation for DocumentCreator {
    type Parent = User;
    type Child = Document;
    type ForeignKey = UserId;

    fn foreign_key(document: &Document) -> Option<UserId> {
        Some(document.created_by)
    }
}
```

The marker type is simultaneously:

- the semantic identity of the relationship;
- the monomorphized foreign-key accessor;
- the physical index identity;
- the preferred partition identity;
- the proof that repeated uses may share an index.

Optional relationships return `None`; mandatory relationships return `Some`.
This avoids encoding optionality by changing the join-key type to `Option<K>`.

The parent entity type is semantic identity; the physical join targets its
typed map-key space through `IdFor<Parent>::MapKey`. It deliberately does not
require the left query's current value to equal `Parent`. A query may enrich or
reshape a parent value with `map_values` between joins while retaining the same
map key, and subsequent FK joins continue to type-check and preserve their
partition. The typed keyspace, not the current payload shape, proves the join
target.

### Join surface

Associated `MapQuery` types allow the relationship to be the only named generic
at common FK join sites:

```rust
fn left_join_fk<R>(
    self,
    right: impl MapQuery<Value = R::Child>,
) -> impl MapQuery<Key = Self::Key, Value = (Self::Value, Vec<R::Child>)>
where
    R: ForeignKeyRelation,
    R::ForeignKey: IdFor<R::Parent, MapKey = Self::Key>;
```

Usage:

```rust
users.left_join_fk::<UserPosts>(posts)
```

Equal-map-key joins remain type-directed without a relation marker. Arbitrary
extractor joins remain as an explicit slow/general escape hatch, named to make
the loss of reusable relationship identity visible. They are not the default
API used by schema-generated code.

### Relationship indexes and `NestedMap`

`NestedMap` already represents a materialized live foreign-key index with
forward and reverse mappings. The new compiler should unify that concept with
query-plan relationship indexes rather than introduce an unrelated index
abstraction:

- `ForeignKeyRelation` describes an index semantically.
- A query runtime builds a private typed index for each required
  `(root source, relationship)` pair.
- Repeated joins inside one materialized plan reuse that index.
- A separately materialized/indexed public view is the evolved role of
  `NestedMap`.
- Existing `ReactiveMap`-only `NestedMap` behavior must either become a
  `MapQuery` source or be replaced during the v3 migration.

Relation identity may be inspected during setup to intern repeated index
requirements, but hot updates hold direct typed handles. There is no `TypeId`
or registry lookup per row or per diff.

## Static compiler

### Plan and runtime traits

Replace recursive `MapQueryInstall`/`MapDiffSink` installation with an internal
compiler whose associated runtime remains concrete:

```rust
pub(crate) trait CompileQuery: MapQuery {
    type Runtime: QueryRuntime<Key = Self::Key, Value = Self::Value>;

    fn compile(self, cx: &mut CompileContext) -> Self::Runtime;
}

pub(crate) trait QueryRuntime: Send + Sync + 'static {
    type Key: Hash + Eq + CellValue;
    type Value: CellValue;

    fn install_roots(
        self: Arc<Self>,
        output: WeakCellMap<Self::Key, Self::Value>,
    ) -> Vec<SubscriptionGuard>;
}
```

The exact ownership surface may change during the spike. The invariant is that
the runtime type and stage calls remain statically known. `Arc<Self>` at the
outer materialized lifetime boundary is acceptable; `Arc<dyn Fn>` between
logical stages is not.

Each operator contributes a concrete runtime component. Conceptually:

```rust
impl<S, F> CompileQuery for SelectPlan<S, F>
where
    S: CompileQuery,
{
    type Runtime = SelectRuntime<S::Runtime, F>;
}
```

In practice, key-preserving stateless operators should compile into a generic
sink/kernel composition rather than allocate a stateful `SelectRuntime` object.
LLVM must see direct calls through a fused region and be able to inline the
predicate and projection bodies.

### Multiple roots

A multi-join plan is a typed tree with several source roots. Compilation gives
each root a direct entry point into the earliest stage it can affect:

```text
left root  ----------------> join_1 -> map -> join_2 -> output
join_1 right root ---------> join_1 -> map -> join_2 -> output
join_2 right root --------------------------> join_2 -> output
```

A change on the final right root does not run earlier joins. All entry points
operate on one coordinated plan state (or its partition-local shard), not on a
chain of separately synchronized stage runtimes.

### Fused regions

The compiler fuses consecutive operators while their partitioning and state
requirements permit it:

```text
select -> map_values -> select -> filter_map_values
```

becomes one key-preserving kernel. An input row is carried in registers or
stack temporaries through the region. No intermediate diff, collection, output
cache, or subscriber callback exists between these operations.

A join is a stateful boundary but not necessarily a dispatch or publication
boundary. Its emitted affected rows flow directly into the following fused
region and subsequent join.

Rekeying may create a repartition boundary. It does not force an observable
`CellMap` or a dynamic callback boundary.

### State and scratch

The compiled runtime owns:

- one final output cache;
- the minimum state required by each stateful operator;
- one instance of every reusable relationship index;
- reusable impacted-key sets and output buffers;
- deterministic input/output ordinal storage;
- sequential and parallel execution configuration.

Stateless operators own no source mirror or output cache. Scratch storage is
cleared and reused rather than allocated for every diff. Small cardinalities
use inline zero/one storage before falling back to a vector.

### Compilation-resource policy

The default query runtime is fully monomorphized. Large static types, longer
builds, larger artifacts, and increased rustc memory are accepted costs.

An explicitly erased escape hatch may be added later:

```rust
query.erase()
query.boxed()
```

It is not the default, and the first implementation does not need it. The
isolated deep-query benchmarks continue to build serially so CI and developer
machines have a bounded validation path even when application builds choose
much larger plans.

## Deterministic multicore execution

### Principle

Determinism constrains publication, not internal instruction scheduling. A
batch may compute concurrently as long as the observable output is identical,
ordered, and fully settled when the initiating call returns.

```text
input batch with stable ordinals
        v
partition by current key/relation
        v
execute each shard through the longest possible compiled region
        v
barrier at a true repartition or final commit boundary
        v
stable merge by input ordinal and per-input emission ordinal
        v
publish output diffs synchronously
```

### Shard ownership

Each worker receives exclusive mutable access to a typed shard state. Local
indexes and caches therefore need no mutex during shard execution. Shared
sources are partitioned by the relationship or map key used by the next
stateful stage.

```rust
struct ParallelRuntime<P>
where
    P: CompiledPlan,
{
    shards: Vec<P::ShardState>,
    config: ParallelConfig,
}
```

The representation may use Rayon parallel iterators over disjoint shard slices
or scoped tasks on Hyphae's dedicated wave pool. It must not spawn one task per
row and must not create a new thread pool per materialization.

### Partition behavior

- `select`, `select_by`, `map_values`, and `filter_map_values` preserve the
  current partition.
- A relationship join wants both relevant sides partitioned by its relation
  key.
- A rekeying projection continues locally only when the next stage consumes
  the new key partition; otherwise it emits into deterministic destination
  shard buffers.
- A join chain with different relationship keys may require a shuffle between
  joins. The compiler fuses everything on either side of that shuffle.
- Independent root changes entering the same active batch are ordered by their
  assigned input ordinal before final publication.

### Adaptive threshold

Parallelism is chosen using estimated work, not row count alone:

```text
estimated work = changed rows * compiled plan cost
```

Plan cost accounts for joins, fan-out, expected index probes, rekeys, and user
kernel hints where available. Initial thresholds are benchmark-derived
constants. Runtime sampling or adaptive calibration is out of scope until a
static model is measured insufficient.

Required behavior:

- a one-row diff stays on the calling thread;
- a large trivial selection may remain sequential;
- a smaller batch crossing several joins may parallelize;
- hysteresis prevents adjacent batch sizes from repeatedly switching modes;
- parallel dispatch overhead is included in benchmark gates.

### Rayon integration

Use Hyphae's existing dedicated scheduler Rayon pool. Do not use the process
global pool implicitly. Nested scheduler/query execution must reuse the same
pool and avoid oversubscription.

The current pool is private to the feature-gated scheduler module. Move its
ownership behind a shared crate-private native executor so scheduler waves and
compiled queries use the same workers. With the `scheduler` feature disabled,
or on wasm, compiled queries use the sequential runtime. Enabling parallel
query execution must not silently create a second pool.

The scheduler may continue to parallelize independent same-height scalar cells.
A compiled map query appears to the scheduler as one coarse reactive node while
using its own plan-visible partition parallelism internally. This prevents the
scheduler from attempting to discover parallelism hidden behind one stateful
callback.

### Synchronous contract

The public contract remains:

```rust
map.insert_many(changes);
// The compiled query has completed parallel work, deterministically committed,
// notified its output subscribers, and settled before this line runs.
```

No future, async executor, or eventual-consistency surface is introduced.

## Observable semantics

The compiler must preserve:

1. Initial state is published before later live diffs can overtake it.
2. An insert/update/remove produces the same final map as sequential source
   order.
3. Batch members retain their logical input order at publication.
4. Multiple outputs from one input retain operator-defined local order.
5. Completion and error delivery do not overtake prior values.
6. Dropping the final materialized map tears down every root subscription and
   all plan state.
7. Reentrant source changes do not observe partially committed output.
8. A panic in one parallel partition follows the existing scheduler panic
   policy and cannot strand the runtime in an active batch.
9. Stateful event semantics are never silently coalesced.
10. Sequential and parallel execution are differential-test equivalent.

Hash-map iteration order is not an output-order contract. Deterministic order
comes from explicit input and emission ordinals, not from sorting keys (which
would require `Ord`) or relying on hash-table layout.

## API migration

This is an intentional breaking v3 change.

| Existing surface | Replacement |
| --- | --- |
| `MapQuery<K, V>` | `MapQuery<Key = K, Value = V>` |
| `HasForeignKey<Parent>` | `ForeignKeyRelation` marker types |
| `project` | `map_values`, `filter_map_values`, or `map_entries` |
| `project_many` | `flat_map_entries` or a grouping operator |
| `left_join_by` for schema relationships | `left_join_fk::<Relation>` |
| `inner_join_by` for schema relationships | `inner_join_fk::<Relation>` |
| `NestedMap` as separate `ReactiveMap` facility | compiled/materialized relationship index |

General extractor joins remain available for truly ad hoc relationships, but
generated/schema-owned code should emit relationship markers. Migration should
prefer the most specific projection operator instead of mechanically replacing
every `project` with `map_entries`.

## Mandatory before/after benchmark discipline

Every implementation phase begins by capturing its own pre-change baseline and
ends by running the identical benchmark suite against the candidate. A phase is
not complete, and no performance claim is accepted, without both sides of the
comparison.

The baseline must be captured before modifying the code exercised by that
phase. If implementation has already begun, reproduce the baseline from the
phase's parent commit in a separate worktree rather than treating an older,
adjacent benchmark as equivalent.

Each before/after pair uses:

- the same machine, power/performance mode, Rust toolchain, Cargo profile,
  feature set, dependency lockfile, and benchmark command;
- an idle machine without competing Cargo/rustc work;
- Criterion confidence intervals for latency and throughput;
- deterministic allocation counts and bytes for setup and steady-state work;
- retained-byte measurements after materialization and teardown;
- peak rustc RSS, compile wall time, and artifact size for the representative
  static plan;
- correctness counters proving both candidates performed the same logical
  work and produced the same output cardinality.

Archive a checked-in report for every phase containing:

```text
baseline commit and candidate commit
exact commands
hardware/toolchain/environment metadata
raw benchmark summaries
before/after table with confidence intervals
allocation and retained-memory table
compile-resource table
correctness counters
interpretation and go/no-go decision
```

Criterion baseline names include the phase and baseline commit so results
cannot be silently reused after the workload changes. Benchmark harness changes
are committed separately and run against both revisions whenever possible. If
a breaking API makes one source file unable to compile on both revisions, keep
semantically identical revision-specific adapters and document the difference.

Before/after comparisons are required even when the expected result is a
correctness or memory win rather than lower latency. A regression may be
accepted only when the report names it, quantifies it, and demonstrates that it
is inside that phase's stated gate.

## Implementation sequence

Each phase is independently benchmarked and may land separately. Do not begin
with parallel joins. For every phase below, “complete” includes its mandatory
before/after report.

### Phase 0: benchmark and correctness harness

1. Capture and archive the untouched current-v3 baseline before changing any
   query runtime or public operator.
2. Extend allocation profiling to count steady-state allocations by operator
   shape, not only whole reactive graphs.
3. Add representative plans:
   - `select -> map_values -> select`;
   - two joins separated by `map_values`;
   - four FK joins with two repeated relationships;
   - a rekey between joins;
   - initial snapshots and batches at 1, 10, 100, 1,000, and 10,000 rows.
4. Record compile time, peak rustc RSS, artifact size, runtime latency,
   allocations, and retained bytes.
5. Add a sequential reference evaluator used by differential tests.
6. Re-run the harness against its own parent revision when harness adapters are
   needed, proving the baseline measures equivalent work.

### Phase 1: semantic API and stateless operators

1. Convert `MapQuery` to associated key/value types.
2. Introduce the specific projection vocabulary.
3. Implement stateless `select`, `select_by`, `map_values`, and
   `filter_map_values` directly from incoming old/new diffs.
4. Remove `source_rows` and `source_output_keys` from those runtimes.
5. Preserve the current installer behind the new API until Phase 2.

This phase must close or supersede `lv-c682`.

### Phase 2: typed runtime compiler

1. Introduce `CompileQuery` and concrete associated runtimes.
2. Replace `MapDiffSink = Arc<dyn Fn>` between stateless stages with generic
   typed kernel composition.
3. Give each root a direct typed entry point.
4. Fuse key-preserving projection regions.
5. Retain sequential execution and existing join state initially.

### Phase 3: relationship markers and typed indexes

1. Replace `HasForeignKey<Parent>` with `ForeignKeyRelation`.
2. Port FK joins to relation markers.
3. Unify `NestedMap` with materialized relation indexes.
4. Intern repeated `(root source, relation)` requirements during compilation.
5. Ensure hot paths hold direct typed index handles.

### Phase 4: coordinated multi-join state

1. Move per-stage join locks into one plan transaction boundary.
2. Reuse plan scratch across stages.
3. Route each root directly to its first affected join.
4. Carry affected rows directly through downstream projection/join stages.
5. Add explicit repartition boundaries for rekeying and changed join keys.

### Phase 5: deterministic sharded execution

1. Add stable input/emission ordinals and deterministic merge.
2. Shard plan state by map or relation key.
3. Execute large batches on the existing dedicated Rayon pool.
4. Add the static work-cost threshold and sequential fallback.
5. Differential-test sequential versus parallel execution under randomized
   mixed diffs, reentrancy, panic, and concurrent-root stress.

### Phase 6: advanced physical optimization

Only after fresh profiles:

- projection pushdown using generated field-access metadata;
- join reordering;
- cardinality statistics;
- cross-materialization index sharing;
- explicit erased/boxed plans for compile-resource-sensitive consumers.

## Performance gates

All comparisons run on the same machine and revision profile. Confidence
intervals, allocation counts, and peak RSS are recorded in a checked-in report.

### Phase 1 gates

- A selective `select` retains memory proportional to matched rows, not source
  rows, aside from the final output map's required storage.
- One-row steady-state `select`/`map_values` updates allocate zero temporary
  collections.
- Existing key-preserving query latency does not regress by more than 5%.

### Phase 2-4 gates

- A four-stage key-preserving region performs no indirect stage dispatch and no
  intermediate diff allocation (verified by allocation harness and generated
  code/profile inspection).
- A multi-join update acquires at most one plan-level synchronization boundary
  on the sequential path.
- Two uses of the same `(source, relation)` maintain one relationship index.
- Two- and four-join application-shaped plans improve update latency by at
  least 30% over the current v3 installer.

### Phase 5 gates

- Sequential mode stays within 5% of the Phase 4 sequential runtime for tiny
  updates.
- Parallel mode beats sequential mode by at least 1.5x at its enabled
  threshold and scales positively through the machine's useful physical-core
  count on large join batches.
- Parallel output is byte-for-byte/order-for-order equivalent to the reference
  evaluator across randomized differential tests.
- `insert_many` remains synchronously settled on return.

Compile time and peak rustc RSS are reported but are not release blockers unless
they prevent the isolated benchmark from compiling serially on the project's
CI machine.

## Risks

### Compile-resource explosion

This is accepted strategically, but must remain operationally measurable.
Deep-query benchmarks stay isolated and serial. Generated code size and
instruction-cache behavior are measured because more monomorphization can
eventually make runtime slower even when compile cost is unconstrained.

### Rust specialization limitations

Stable Rust cannot generally pattern-match adjacent generic plan types through
overlapping trait implementations. The design avoids depending on magical
specialization: distinct semantic operators compile compositionally, and
cross-stage optimization is expressed through their declared properties and
compiler context. If a specific fusion cannot be expressed coherently, add an
explicit plan node or generated implementation rather than falling back to an
opaque hot closure.

### Fine-grained parallel overhead

Rayon task submission, repartition buffers, and barriers can overwhelm small
updates. The sequential path is permanent, not a temporary fallback, and every
parallel boundary must clear a measured crossover gate.

### Reentrancy and partial commit

User callbacks run only after a deterministic commit. No subscriber observes
one shard committed while another is still computing. Reentrant mutations join
the next propagation transaction rather than mutating an in-flight shard.

### Index sharing aliases mutable state

Shared relationship indexes are plan-owned physical state, not independently
locked callbacks. The compiler must prevent two parallel tasks from mutating
the same shard of a shared index concurrently. Partition ownership is the
primary safety mechanism; a global mutex per shared index would defeat the
design.

## Resolved decisions

- Runtime performance takes priority over compile time and peak rustc memory.
- Default execution remains statically typed and monomorphized.
- Rayon remains the native parallel backend through Hyphae's dedicated pool.
- Publication remains synchronous and deterministic.
- The API will expose more specific projection operators.
- Relationship types replace parent-only foreign-key identity.
- Relationship types identify indexes and partitions as well as semantic FKs.
- Closures remain for payload computation, not structural relationship facts.
- `materialize()` remains the single public compilation/observation boundary.
- Every implementation phase requires checked-in before/after benchmarks from
  equivalent workloads before it can be declared complete.

## Open decisions required before implementation

1. Exact error-delivery behavior for a `map_entries` unique-key collision.
2. Whether the first API migration removes `project` immediately or retains it
   for one deprecation cycle on the v3 branch.
3. Whether `NestedMap` keeps its name for the public materialized relationship
   index or is replaced by `RelationIndex`/`IndexedMap`.
4. Whether schema generators emit relation marker structs directly or derive
   them from field annotations.
5. The precise internal trait shape needed to give several source roots direct
   typed entry points without placing dynamic dispatch between stages.

These do not change the architectural decision. They are resolved during the
Phase 0/1 implementation plan and API spike.
