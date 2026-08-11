# Statically Compiled MapQuery Engine

**Date:** 2026-08-10
**Status:** Phases 1–5 implemented at runtime `5177ef5` with recorded deviations/deferred scope; final resource evidence archived at `db1459a`
**Tracking:** `lv-db06` implementation (`lv-d884` design origin)
**Supersedes:** The Phase B optimizer direction in `2026-04-24-cell-map-query-plans-design.md`

## Decision

Hyphae's collection-query API describes relational facts in types and compiles
each materialized query into a fully monomorphized incremental program.
The default runtime favors update latency and efficient parallel work even when
that substantially increases rustc time, memory use, and generated code size.

Rayon is the execution backend, not the query architecture. A compiled plan
identifies coarse, partition-local regions; Rayon executes those regions over
exclusive shards. Externally visible publication remains synchronous,
deterministic, and ordered.

The implemented default flow is:

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

## Historical v3 baseline architecture (superseded)

At the frozen Phase-0 runtime `7fcb1421`, `MapQuery` had already removed
intermediate observable `CellMap`s. A query plan was installed recursively,
however, and every stage wrapped
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

The breaking v3 window replaced `MapQuery<K, V>` with associated types.
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

The general `project`/`project_many` names hid properties the compiler would
otherwise have to guess. They were replaced with operators whose types state
cardinality and key behavior:

| Operator | Cardinality | Output key | Partition effect |
| --- | ---: | --- | --- |
| `select` | 1:0/1 | unchanged | preserved |
| `select_by` | 1:0/1 | unchanged | preserved |
| `map_values` | 1:1 | unchanged | preserved |
| `filter_map_values` | 1:0/1 | unchanged | preserved |
| `map_entries` | 1:1 | may change | repartition boundary |
| `filter_map_entries` | 1:0/1 | may change | repartition boundary |
| `flat_map_entries` | 1:N | `(source key, local key)` | repartition boundary |

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

Two source rows mapping to one output key never silently overwrite each other:

- Key-preserving operators are collision-free by construction.
- Many-to-one transformations use an explicit grouping/aggregation operator.
- `map_entries` and `filter_map_entries` require globally unique output keys.
  The runtime validates ownership before output mutation and panics
  synchronously on collision; there is no structured map-query error channel.
- `flat_map_entries` identifies output by `(source_key, local_key)`, preventing
  cross-source collisions. A source row must still emit each local key once.

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

The former `HasForeignKey<T>` identified a foreign key only by its parent
entity type. Rust coherence permits only one such implementation for a given
child/parent pair, so it cannot represent both `Document.created_by -> User`
and `Document.updated_by -> User`.

It was replaced by a zero-sized relationship marker:

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
fn left_join_fk<Rel, R>(
    self,
    right: R,
) -> impl MapQuery<Key = Self::Key, Value = (Self::Value, Vec<Rel::Child>)>
where
    Rel: ForeignKeyRelation,
    R: MapQuery<Value = Rel::Child>,
    Rel::ForeignKey: IdFor<Rel::Parent, MapKey = Self::Key>;
```

Usage:

```rust
users.left_join_fk::<UserPosts, _>(posts)
```

Equal-map-key joins remain type-directed without a relation marker. Arbitrary
extractor joins remain as an explicit slow/general escape hatch, named to make
the loss of reusable relationship identity visible. They are not the default
API used by schema-generated code.

### Relationship indexes and `NestedMap`

The implemented compiler interns one typed relationship index for each repeated
`(raw physical right source, ForeignKeyRelation)` pair in one materialized
plan. Transformed or filtered right plans deliberately receive private indexes,
because sharing them would conflate different row sets. Hot updates hold direct
typed handles; there is no per-row `TypeId` registry lookup.

`NestedMap` remains a separate public, closure-indexed, read-only grouped view.
It implements `ReactiveMap` and is a `MapQuery` source. Unifying its storage or
name with compiler-private relationship indexes is deferred; current semantics
do not claim that unification.

## Static compiler

### Plan and runtime traits

The implemented compiler consumes a sealed plan while preserving a concrete
associated runtime. Ordinary stage edges use generic sinks; erasure is limited
to physical root fanout and explicit share boundaries:

```rust
pub(crate) trait CompileQuery<K, V> {
    type Runtime: QueryRuntime<Key = K, Value = V>;
    fn compile(self, cx: &mut CompileContext) -> Self::Runtime;
}

pub(crate) trait BuildQueryRuntime<K, V> {
    fn build_into<S: MapDiffSink<K, V>>(
        self,
        cx: &mut CompileContext,
        sink: S,
    ) -> Vec<SubscriptionGuard>;
}

pub(crate) trait QueryRuntime {
    type Key: CellValue + Hash + Eq;
    type Value: CellValue;
    fn connect<S: MapDiffSink<Self::Key, Self::Value>>(
        self,
        cx: &mut CompileContext,
        sink: S,
    ) -> Vec<SubscriptionGuard>;
}
```

The blanket `PlanRuntime<P, K, V>` retains `P`'s concrete type. Compilation
registers every physical root first, then `QueryRuntime::install_roots`
activates the completed graph once. Key-preserving stateless operators and
recognized join regions compose as concrete kernels rather than independently
allocated stateful stage objects.

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
kernel hints where available. The calibrated estimated-work hysteresis enters parallel mode at **200,000**
and exits below **96,000**. A balance gate rejects work whose shard distribution
would make dispatch unproductive; the runtime requests the pool only after cost
and balance eligibility are established.

Required behavior:

- a one-row diff stays on the calling thread;
- a large trivial selection may remain sequential;
- a smaller batch crossing several joins may parallelize;
- hysteresis prevents adjacent batch sizes from repeatedly switching modes;
- parallel dispatch overhead is included in benchmark gates.

### Rayon integration

Eligible native join regions and scheduler waves use the same lazily created,
dedicated Rayon pool; neither uses the process-global pool or creates a second
pool. Parallel map-query dispatch is compiled only with `scheduler`. Wasm and
builds without that feature execute sequentially. `HYPHAE_WORKER_THREADS`
configures the pool (zero disables it), with `HYPHAE_WAVE_THREADS` as a
compatibility fallback and a default cap of four workers.

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

The implemented compiler preserves:

1. Initial state is published before later live diffs can overtake it.
2. An insert/update/remove produces the same final map as sequential source
   order.
3. Batch members retain logical input order at publication, including nested
   `Initial` barriers and repeated/non-unique members.
4. Multiple outputs from one input retain operator-defined local order.
5. `ReactiveMap` carries `MapDiff`, not completion/error terminals; terminal
   completion/error ordering is currently non-applicable.
6. Dropping the final materialized map tears down every root subscription and
   all plan state.
7. Reentrant root changes queue behind the active query transaction.
8. A panic during a coordinated promoted `JoinRegion` application joins sibling
   workers, publishes none of that failed region apply, clears queued/reentrant
   region work, and poisons the query cohort. Later physical-root callbacks
   panic with `"hyphae join region is poisoned after a prior callback panic"`.
   This is not rollback of source state, earlier publications/subscriber side
   effects, or ordinary non-region callback panics.
9. Stateful event semantics are never silently coalesced.
10. Sequential and parallel execution are differential-test equivalent.
11. The initiating source mutation returns only after deterministic caller-side
    merge, output publication, and synchronous subscriber settlement.

Hash-map iteration order is not an output-order contract. Deterministic order
comes from explicit input and emission ordinals, not sorting keys (which would
require `Ord`) or relying on hash-table layout.

## API migration

This is an intentional breaking v3 change.

| Existing surface | Replacement |
| --- | --- |
| `MapQuery<K, V>` | `MapQuery<Key = K, Value = V>` |
| `HasForeignKey<Parent>` | `ForeignKeyRelation` marker types |
| `project` | `map_values`, `filter_map_values`, `map_entries`, or `filter_map_entries` according to cardinality/key behavior |
| `project_many` | `flat_map_entries` with new `(source_key, local_key)` identity, or redesign around explicit grouping |
| `left_join_by` for schema relationships | `left_join_fk::<Relation, _>` |
| `inner_join_by` for schema relationships | `inner_join_fk::<Relation, _>` |
| `NestedMap` | no replacement; it remains a separate public grouped view (`ReactiveMap` + `MapQuery` source) |

General extractor joins remain available for truly ad hoc relationships, but
generated/schema-owned code should emit relationship markers. Migration should
prefer the most specific projection operator instead of mechanically replacing
every `project` with `map_entries`.

## Mandatory before/after benchmark discipline

Every new implementation phase begins by capturing its own pre-change baseline
and ends by running the identical benchmark suite against the candidate. A
phase-local performance claim is not accepted without both sides of that
comparison.

The baseline must be captured before modifying the code exercised by that
phase. If implementation has already begun, reproduce the baseline from the
phase's parent commit in a separate worktree rather than treating an older,
adjacent benchmark as equivalent. If exact historical phase-local capture is
no longer possible, do **not** manufacture retrospective provenance: record the
missing phase tuple as an explicit process deviation, withdraw independent
performance claims for that checkpoint, and use an exact-checkout consolidated
baseline/candidate comparison for release gates. Release closure then requires
a checked reconciliation mapping every phase to its available report and the
consolidated evidence.

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

Archive a checked-in report for every phase containing the following fields, or
record the missing historical report explicitly in the release reconciliation:

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

Each phase should be independently benchmarked and may land separately. Do not
begin with parallel joins. For every phase below, “complete” includes its
before/after report or an explicit release-reconciliation deviation that makes
no unsupported phase-local performance claim.

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
3. Keep `NestedMap` as the public grouped view; storage unification with private relation indexes is deferred (recorded deviation).
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
   mixed diffs, reentrancy, and concurrent-root stress. Exercise panic in
   separately forced sequential and parallel fail-stop cases: after a panic the
   cohort is terminal, so there is intentionally no continuing state oracle.

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

## Resolved and deferred decisions

- Runtime performance takes priority over compile time and peak rustc memory;
  serial compile resource use is recorded rather than treated as a release gate.
- Default execution is statically typed and monomorphized. Physical root
  registries and explicit share points are the intentional erased boundaries.
- `CompileQuery`/`BuildQueryRuntime`, concrete `PlanRuntime`, `QueryRuntime`, and
  compile-before-activate `CompileContext` root registration are the selected
  internal shape.
- Rayon remains the native backend through Hyphae's one dedicated shared pool;
  publication remains synchronous, deterministic, and caller-thread merged.
- Semantic projection operators replaced `project`/`project_many` immediately.
- `map_entries`/`filter_map_entries` collisions panic synchronously after
  pre-mutation validation; `MapDiff` has no structured error terminal.
- Named relationship types identify semantic FKs, partitions, and indexes.
  Schema owners/generators emit marker structs and implementations; annotation
  syntax belongs to an upstream generator, not this crate.
- `NestedMap` keeps its name and separate public grouped-view role. Storage
  unification with compiler-private relationship indexes is deferred.
- Closures remain for payload computation, not structural relationship facts,
  and have the documented pure/concurrent invocation contract.
- `materialize()` remains the single public compilation/observation boundary.
- Advanced pushdown, join reorder/statistics, cross-materialization index
  sharing, and an opt-in boxed plan are deferred Phase-6 work.
- The historical Phase-3 series has no standalone full-tuple report, and the
  early Phase-4 report lacks some resource cells. Those checkpoints make no
  independent release performance claim; the checked release reconciliation
  and exact frozen v3/final comparison are authoritative.

## Final conformance record

| Requirement | Status | Evidence / exact scope |
| --- | --- | --- |
| Associated `MapQuery` key/value | Conforms | `map_query::MapQuery::{Key, Value}`. |
| Semantic projections and typed properties | Conforms | `MapValuesExt`, `MapEntriesExt`, `FlatMapEntriesExt`, `SelectExt`, and `PlanProperties`. |
| Collision-safe rekeys | Conforms with panic surface | `map_runtime` validates unique ownership before mutation; no last-writer-wins or error terminal. |
| Pure closure/invocation contract | Conforms by contract | Bounds are `Fn + Send + Sync`; purity is documented, not type-enforced; order/count/thread are unspecified. |
| Required/optional FK semantics | Conforms by typed-relation convention | Public typed FK extraction returns `Option`: required relations promise always-`Some`; optional `None` rows are omitted. Both use optional extraction into the parent map-key space. |
| Repeated relationship-index reuse | Conforms for raw physical rights | `CompileContext`, `DeferredPhysical`, and `SharedRelationIndex`; transformed rights intentionally stay private. |
| `NestedMap` unification | Partial / deferred | Already a `ReactiveMap` and `MapQuery` source; public closure-indexed storage stays separate. |
| Static compiler and monomorphized edges | Conforms with boundary erasure | `CompileQuery::Runtime`, `BuildQueryRuntime`, and `QueryRuntime`; root fanout/share boundaries erase. |
| Stateless/key-preserving fusion | Conforms for recognized shapes | Fused projection kernels; universal algebra fusion is not claimed. |
| Coordinated arbitrary-N joins | Conforms for recognized fluent left-join regions | Third join promotes to `JoinRegion`; typed N=3/N=8 differential coverage; rekeys are boundaries. |
| Deterministic adaptive Rayon | Conforms for eligible join runtimes | Native + `scheduler`, 200,000/96,000 hysteresis, balance gate, shared pool; sequential otherwise. |
| Synchronous settlement | Conforms | Workers compute only; caller merges/publishes before source mutation returns. |
| Completion/error ordering | Not applicable | `ReactiveMap` exposes `MapDiff` and no terminal completion/error channel. |
| Panic safety | Conforms to narrow `JoinRegion` fail-stop | Siblings join, failed apply publishes none, cohort poisons; no source/subscriber/global rollback. |
| Teardown | Conforms | Output owns root guards and final sink is weak; drop tears installation down. |
| Phase-5 differential/order | Conforms by split coverage | Randomized forced sequential/shard/Rayon state and exact trace oracle at `3d4db44`; separately forced serial/parallel terminal panic proofs at `f6e2b57`. |
| Enabled-threshold scaling | Conforms | Exact first-enabled-row W1/W2/W4 confidence-bound floors are at least 1.660x in `benchmark-results/lv-515f/`. |
| Performance/resource gates | Conforms at release level; historical process deviation recorded | Final latency/scaling plus allocations/codegen/serial compile report in `benchmark-results/lv-671e/`; phase mapping and missing Phase-3/4 tuple cells in `docs/superpowers/plans/2026-08-11-static-map-query-phase-evidence-reconciliation.md`. |
| Advanced physical optimization | Deferred | Pushdown/reorder/stats/cross-materialization sharing/boxed plans remain Phase 6. |
