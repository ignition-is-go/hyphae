//! Uncompiled, statically typed reactive-map operation chains.
//!
//! A [`MapQuery`] is a consuming recipe whose associated [`MapQuery::Key`] and
//! [`MapQuery::Value`] types describe its output. Joins, semantic projections,
//! and selections compose without an intermediate observable [`CellMap`].
//! [`MapQuery::materialize`] is the sole observation boundary: it compiles the
//! concrete plan, registers each interned physical root before activation, and
//! returns a cached, subscribable output map.
//!
//! Operator plans and kernels remain statically typed. Runtime diff edges use a
//! fixed erased callback shape so a downstream continuation cannot recursively
//! multiply the concrete types of every upstream branch. Recognized
//! key-preserving and fluent left-join regions still fuse; rekeys and unsupported
//! algebra shapes remain physical boundaries.
//!
//! # Closure and publication contract
//!
//! Query closures must be deterministic, externally side-effect-free, and
//! nonblocking. The runtime may invoke them repeatedly or concurrently;
//! invocation count, order, and thread are not API guarantees. Output diffs
//! are merged and published deterministically and synchronously: the initiating
//! source mutation does not return before query publication and its synchronous
//! subscribers settle.
//!
//! [`ReactiveMap`](crate::traits::ReactiveMap) exposes [`MapDiff`] values, not
//! scalar `Signal` completion/error terminals. Completion/error ordering is
//! therefore not applicable to the current map-query surface.
//!
//! # Panic and teardown contract
//!
//! A panic during one application of a coordinated promoted `JoinRegion` joins
//! sibling workers, publishes none of that failed region application's changes,
//! clears queued/reentrant region work, and poisons all physical roots in the
//! materialized query cohort. A later root callback panics with
//! `"hyphae join region is poisoned after a prior callback panic"`.
//! This fail-stop boundary does **not** roll back the source mutation, output or
//! subscriber work published before the callback, subscriber side effects, or
//! ordinary callbacks outside a promoted region.
//!
//! The materialized output owns the installed root guards while the final sink
//! holds the output weakly. Dropping the output tears down the installation.

use std::{collections::HashMap, hash::Hash, marker::PhantomData, sync::Arc};

use parking_lot::Mutex;

use crate::{
    cell::{CellImmutable, CellMutable},
    cell_map::{CellMap, MapDiff},
    subscription::SubscriptionGuard,
    traits::CellValue,
};

pub(crate) mod properties;
pub(crate) mod reactive_map_impl;
pub mod share;

pub(crate) mod compiler;

pub use share::{MapQueryShareExt, SharedMapQuery};

/// Type-erased downstream diff consumer used on runtime edges.
///
/// Plans and sources remain statically typed, while erasing the callback keeps
/// sink types from recursively multiplying across binary DAG branches.
pub(crate) type BoxedMapDiffSink<K, V> = Arc<dyn Fn(&MapDiff<K, V>) + Send + Sync>;

/// Internal builder implemented by each sealed plan node. Builders compose
/// statically and are consumed into a concrete [`QueryRuntime`] by
/// [`CompileQuery::compile`].
pub(crate) trait BuildQueryRuntime<K, V>: Sized + Send + Sync + 'static
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    fn build_into(
        self,
        cx: &mut compiler::CompileContext,
        sink: BoxedMapDiffSink<K, V>,
    ) -> Vec<SubscriptionGuard>;

    /// Stable identity is available only at an untransformed physical source.
    /// Operator nodes intentionally inherit `None`: sharing relationship state
    /// across filtered or projected inputs would conflate distinct semantics.
    fn raw_source_identity(&self) -> Option<compiler::SourceIdentity> {
        None
    }
}

/// Concrete runtime produced by compiling a query plan.
///
/// The runtime retains its exact plan type and statically typed operator state.
/// Diff callbacks between runtime nodes have one erased shape, preventing sink
/// types from growing multiplicatively across branched plans.
pub(crate) trait QueryRuntime: Sized + Send + Sync + 'static {
    type Key: CellValue + Hash + Eq;
    type Value: CellValue;

    /// Connect this runtime to its output without activating roots. This is
    /// used by nested compilation and shared-query ownership boundaries.
    fn connect(
        self,
        cx: &mut compiler::CompileContext,
        sink: BoxedMapDiffSink<Self::Key, Self::Value>,
    ) -> Vec<SubscriptionGuard>;

    /// Connect the completed runtime, then activate all registered roots.
    fn install_roots(
        self,
        cx: &mut compiler::CompileContext,
        sink: BoxedMapDiffSink<Self::Key, Self::Value>,
    ) -> Vec<SubscriptionGuard> {
        let mut guards = self.connect(cx, sink);
        guards.extend(cx.activate());
        guards
    }
}

/// Plan-specific runtime wrapper. `P` remains concrete through materialization.
pub(crate) struct PlanRuntime<P, K, V> {
    plan: P,
    _types: PhantomData<fn() -> (K, V)>,
}

impl<P, K, V> QueryRuntime for PlanRuntime<P, K, V>
where
    P: BuildQueryRuntime<K, V>,
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    type Key = K;
    type Value = V;

    fn connect(
        self,
        cx: &mut compiler::CompileContext,
        sink: BoxedMapDiffSink<K, V>,
    ) -> Vec<SubscriptionGuard> {
        self.plan.build_into(cx, sink)
    }
}

/// Crate-private compiler hook used by [`MapQuery::materialize`].
///
/// Compilation consumes a plan and exposes its concrete associated runtime.
/// Root activation remains a distinct runtime operation after the whole plan
/// has registered its entry points.
pub(crate) trait CompileQuery<K, V>: Sized + Send + Sync + 'static
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    type Runtime: QueryRuntime<Key = K, Value = V>;

    fn compile(self, _cx: &mut compiler::CompileContext) -> Self::Runtime;

    fn raw_source_identity(&self) -> Option<compiler::SourceIdentity>;
}

impl<P, K, V> CompileQuery<K, V> for P
where
    P: BuildQueryRuntime<K, V>,
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    type Runtime = PlanRuntime<P, K, V>;

    fn compile(self, _cx: &mut compiler::CompileContext) -> Self::Runtime {
        PlanRuntime {
            plan: self,
            _types: PhantomData,
        }
    }

    fn raw_source_identity(&self) -> Option<compiler::SourceIdentity> {
        BuildQueryRuntime::raw_source_identity(self)
    }
}

/// Compile a child plan and connect its concrete runtime without activating
/// roots. Activation occurs once, at the outer materialization boundary.
pub(crate) fn compile_runtime_into<Q, K, V>(
    query: Q,
    cx: &mut compiler::CompileContext,
    sink: BoxedMapDiffSink<K, V>,
) -> Vec<SubscriptionGuard>
where
    Q: CompileQuery<K, V>,
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    let runtime = query.compile(cx);
    runtime.connect(cx, sink)
}

/// Uncompiled reactive map operation chain.
///
/// Map queries are built by chaining pure operators on a source ([`CellMap`]
/// or another `MapQuery`). They deliberately do not expose `subscribe` —
/// call [`MapQuery::materialize`] to produce a subscribable [`CellMap`].
///
/// # Invariants
///
/// - `materialize(self)` consumes the plan and installs one subscription per
///   interned physical root.
/// - Plan-to-plan edges are statically typed; recognized regions fuse without
///   an intermediate observable `CellMap`.
/// - Publication is deterministic, ordered, and synchronously settled.
/// - User closures follow the module-level purity and invocation contract.
///
/// # Sealing
///
/// The `CompileQuery<K, V>` supertrait is `pub(crate)`, which seals
/// `MapQuery` so external crates cannot define new query shapes. New plan
/// shapes are added inside this crate.
///
/// # Not `Clone`
///
/// Map queries are deliberately not `Clone`. Cloning would silently
/// duplicate join / projection work — each clone's `materialize()` would
/// install independent root subscriptions and run the entire op chain on
/// every emission. To share work across consumers, materialize once into a
/// [`CellMap`] (which IS `Clone` — the clone is an `Arc` bump referencing
/// the same multicast cache) and then clone the cell map.
#[allow(private_bounds)]
pub trait MapQuery: CompileQuery<Self::Key, Self::Value> + properties::PlanProperties {
    /// Key produced by this query plan.
    type Key: CellValue + Hash + Eq;
    /// Value produced by this query plan.
    type Value: CellValue;

    /// Compile the query into a [`CellMap`] and install root subscriptions
    /// running the statically typed incremental runtime.
    ///
    /// This is the only way to observe map-query output. Every subscription is
    /// on a materialized map, never on a plan. The returned map owns all root
    /// guards; dropping it tears the installation down. Publication caused by
    /// a source mutation is synchronously settled before that mutation returns.
    /// The rows that the roots replay during installation are stored without
    /// a write, so the map is complete when this returns, inside a settle too.
    #[track_caller]
    fn materialize(self) -> CellMap<Self::Key, Self::Value, CellImmutable> {
        let target = Arc::new(Mutex::new(MaterializeTarget::Building(HashMap::new())));

        let sink_target = Arc::clone(&target);
        let sink = move |diff: &MapDiff<Self::Key, Self::Value>| {
            let inner = match &mut *sink_target.lock() {
                MaterializeTarget::Building(rows) => {
                    apply_diff_to_rows(rows, diff);
                    return;
                }
                MaterializeTarget::Built(output) => output.upgrade(),
            };
            let Some(inner) = inner else {
                return;
            };
            let out: CellMap<Self::Key, Self::Value, CellMutable> = CellMap {
                inner,
                _marker: PhantomData,
            };
            // One clone here is unavoidable (subscribe_diffs_reactive passes
            // &diff). apply_diff_owned takes it by value, mutates state, and
            // emits the diff directly via diffs_cell — no Vec, no Batch wrap.
            out.apply_diff_owned(diff.clone());
        };

        let mut cx = compiler::CompileContext::default();
        let runtime = self.compile(&mut cx);
        let guards = runtime.install_roots(&mut cx, Arc::new(sink));
        let mut target = target.lock();
        let entries = match &mut *target {
            MaterializeTarget::Building(rows) => rows.drain().collect(),
            MaterializeTarget::Built(_) => Vec::new(),
        };
        let output = CellMap::<Self::Key, Self::Value, CellMutable>::from_entries(entries);
        // The output owns its guards before the sink can reach it, so its
        // first diff is already placed above every dependency.
        for g in guards {
            output.own(g);
        }
        *target = MaterializeTarget::Built(Arc::downgrade(&output.inner));
        drop(target);
        output.lock()
    }
}

/// Where the sink of [`MapQuery::materialize`] puts a diff.
enum MaterializeTarget<K, V>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    /// The roots are being installed. Nothing can observe the output yet, so
    /// the rows they replay are collected here and the output is built
    /// holding them. Publishing them as diffs would be wrong inside a settle:
    /// the output has no dependency yet, so the diffs would wait for a later
    /// settle, behind an update that joins this one once the output owns its
    /// guards.
    Building(HashMap<K, V>),
    Built(std::sync::Weak<crate::cell_map::CellMapInner<K, V>>),
}

/// Apply `diff` to a plain copy of a map's rows.
pub(crate) fn apply_diff_to_rows<K, V>(rows: &mut HashMap<K, V>, diff: &MapDiff<K, V>)
where
    K: Clone + Hash + Eq,
    V: Clone,
{
    match diff {
        MapDiff::Initial { entries } => {
            rows.clear();
            rows.reserve(entries.len());
            for (key, value) in entries {
                rows.insert(key.clone(), value.clone());
            }
        }
        MapDiff::Insert { key, value } => {
            rows.insert(key.clone(), value.clone());
        }
        MapDiff::Remove { key, .. } => {
            rows.remove(key);
        }
        MapDiff::Update { key, new_value, .. } => {
            rows.insert(key.clone(), new_value.clone());
        }
        MapDiff::Batch { changes } => {
            for change in changes {
                apply_diff_to_rows(rows, change);
            }
        }
    }
}
