//! Uncompiled reactive map operation chains.
//!
//! A [`MapQuery`] is a recipe for a reactive map computation — a chain of
//! pure operators (joins, projections, selections) that has not yet been
//! materialized into a [`CellMap`]. Map queries deliberately do not implement
//! `subscribe`: to observe output you must call [`MapQuery::materialize`],
//! which installs ONE subscription per root source and returns a
//! subscribable cell map.
//!
//! This design makes the memoization boundary explicit. Chaining map operators
//! (`inner_join`, `left_join`, `map_values`, ...) builds a plan without allocating
//! an intermediate [`CellMap`] per stage. The final map cache, diff cell, and
//! per-key cells are allocated only when the caller explicitly asks for them
//! with `.materialize()`.

use std::{hash::Hash, marker::PhantomData, sync::Arc};

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

/// Statically typed downstream diff consumer used while compiling a query.
///
/// The blanket implementation keeps every plan-to-plan edge monomorphized.
/// Type erasure is reserved for actual multicast boundaries and the root
/// source subscriber registry.
pub(crate) trait MapDiffSink<K, V>: Fn(&MapDiff<K, V>) + Send + Sync + 'static {}

impl<K, V, F> MapDiffSink<K, V> for F where F: Fn(&MapDiff<K, V>) + Send + Sync + 'static {}

/// Explicitly erased sink used only by the opt-in shared query boundary.
pub(crate) type BoxedMapDiffSink<K, V> = Arc<dyn Fn(&MapDiff<K, V>) + Send + Sync>;

/// Internal builder implemented by each sealed plan node. Builders compose
/// statically and are consumed into a concrete [`QueryRuntime`] by
/// [`CompileQuery::compile`].
pub(crate) trait BuildQueryRuntime<K, V>: Sized + Send + Sync + 'static
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    fn build_into<S>(self, cx: &mut compiler::CompileContext, sink: S) -> Vec<SubscriptionGuard>
    where
        S: MapDiffSink<K, V>;

    /// Stable identity is available only at an untransformed physical source.
    /// Operator nodes intentionally inherit `None`: sharing relationship state
    /// across filtered or projected inputs would conflate distinct semantics.
    fn raw_source_identity(&self) -> Option<compiler::SourceIdentity> {
        None
    }
}

/// Concrete runtime produced by compiling a query plan.
///
/// The runtime retains its exact plan type, so connecting roots does not erase
/// operator stages behind a trait object. Only the root registry and explicit
/// share boundaries erase callbacks.
pub(crate) trait QueryRuntime: Sized + Send + Sync + 'static {
    type Key: CellValue + Hash + Eq;
    type Value: CellValue;

    /// Connect this runtime to its output without activating roots. This is
    /// used by nested compilation and shared-query ownership boundaries.
    fn connect<S>(self, cx: &mut compiler::CompileContext, sink: S) -> Vec<SubscriptionGuard>
    where
        S: MapDiffSink<Self::Key, Self::Value>;

    /// Connect the completed runtime, then activate all registered roots.
    fn install_roots<S>(self, cx: &mut compiler::CompileContext, sink: S) -> Vec<SubscriptionGuard>
    where
        S: MapDiffSink<Self::Key, Self::Value>,
    {
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

    fn connect<S>(self, cx: &mut compiler::CompileContext, sink: S) -> Vec<SubscriptionGuard>
    where
        S: MapDiffSink<K, V>,
    {
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
pub(crate) fn compile_runtime_into<Q, K, V, S>(
    query: Q,
    cx: &mut compiler::CompileContext,
    sink: S,
) -> Vec<SubscriptionGuard>
where
    Q: CompileQuery<K, V>,
    K: CellValue + Hash + Eq,
    V: CellValue,
    S: MapDiffSink<K, V>,
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
/// - `materialize(self)` consumes the plan and installs ONE subscription per
///   root source running the fully fused diff-propagation closure.
/// - No intermediate `CellMap` is allocated anywhere in a query chain.
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

    /// Compile the query into a [`CellMap`] and install root-source
    /// subscriptions running the fused diff-propagation closure.
    ///
    /// This is the only way to observe map-query output. Every subscribe in
    /// the codebase is on a cell map, never on a query — which is the point.
    #[track_caller]
    fn materialize(self) -> CellMap<Self::Key, Self::Value, CellImmutable> {
        let output = CellMap::<Self::Key, Self::Value, CellMutable>::new();
        let weak = Arc::downgrade(&output.inner);

        let sink = move |diff: &MapDiff<Self::Key, Self::Value>| {
            let Some(inner) = weak.upgrade() else {
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
        let guards = runtime.install_roots(&mut cx, sink);
        for g in guards {
            output.own(g);
        }
        output.lock()
    }
}
