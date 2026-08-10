//! [`MapQuery`] and [`MapQueryInstall`] implementations for reactive-map sources.
//!
//! Every reactive-map source — [`CellMap`], [`NestedMap`] — implements
//! `MapQueryInstall` via a blanket on [`ReactiveMap`] so chained query
//! operators can subscribe to a generic upstream map. `MapQuery<K, V>` is
//! implemented explicitly per source type so that `materialize` can be
//! overridden when a no-op is sound.
//!
//! For [`CellMap`], `materialize` is a marker flip (same `Arc<inner>`, new
//! `PhantomData<CellImmutable>`) — there is no point allocating a fresh
//! cell map + forwarding subscription when the upstream is already a
//! cached, multicast cell map. Concrete plan-node structs (`InnerJoinPlan`,
//! ...) provide their own `MapQuery` impls and inherit the default
//! `materialize`.

use std::{hash::Hash, marker::PhantomData, sync::Arc};

use super::properties::{ByMapKey, ExactlyOne, PlanProperties};
use super::{MapDiffSink, MapQuery, MapQueryInstall};
use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    nested_map::NestedMap,
    subscription::SubscriptionGuard,
    traits::{CellValue, reactive_map::ReactiveMap},
};

fn install_reactive_source<M, S>(
    source: &M,
    identity: super::compiler::SourceIdentity,
    cx: &mut super::compiler::CompileContext,
    sink: S,
) -> Vec<SubscriptionGuard>
where
    M: ReactiveMap + Clone,
    M::Key: CellValue + Hash + Eq,
    M::Value: CellValue,
    S: MapDiffSink<M::Key, M::Value>,
{
    cx.intern_root(identity);
    let keepalive = source.clone();
    let guard = source.subscribe_diffs_reactive(move |diff| {
        let _ = &keepalive;
        sink(diff);
    });
    vec![guard]
}

impl<K, V, M> MapQueryInstall<K, V> for CellMap<K, V, M>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
    M: Clone + Send + Sync + 'static,
{
    fn install<S>(self, cx: &mut super::compiler::CompileContext, sink: S) -> Vec<SubscriptionGuard>
    where
        S: MapDiffSink<K, V>,
    {
        let identity = super::compiler::SourceIdentity::from_ptr(Arc::as_ptr(&self.inner));
        install_reactive_source(&self, identity, cx, sink)
    }
}

impl<PK, K, V> MapQueryInstall<K, V> for NestedMap<PK, K, V>
where
    PK: CellValue + Hash + Eq,
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    fn install<S>(self, cx: &mut super::compiler::CompileContext, sink: S) -> Vec<SubscriptionGuard>
    where
        S: MapDiffSink<K, V>,
    {
        let identity = self.query_source_identity();
        install_reactive_source(&self, identity, cx, sink)
    }
}

impl<M> PlanProperties for M
where
    M: ReactiveMap + Clone,
    M::Key: CellValue + Hash + Eq,
    M::Value: CellValue,
{
    type Cardinality = ExactlyOne;
    type InputPartition = ByMapKey<M::Key>;
    type OutputPartition = ByMapKey<M::Key>;
}

#[allow(private_bounds)]
impl<K, V, M> MapQuery for CellMap<K, V, M>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
    M: Clone + Send + Sync + 'static,
{
    type Key = K;
    type Value = V;

    /// No-op materialize: the cell map is already a cached, multicast source.
    /// Just flip the marker to `CellImmutable` and reuse the same `Arc<inner>`.
    fn materialize(self) -> CellMap<K, V, CellImmutable> {
        CellMap {
            inner: self.inner,
            _marker: PhantomData,
        }
    }
}

#[allow(private_bounds)]
impl<PK, K, V> MapQuery for NestedMap<PK, K, V>
where
    PK: CellValue + Hash + Eq,
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    type Key = K;
    type Value = V;

    // Inherits the default materialize. A NestedMap is not a CellMap; it
    // owns its own diff-stream/state and there is no immutable variant to
    // short-circuit to, so the default allocate-and-forward strategy is
    // correct.
}
