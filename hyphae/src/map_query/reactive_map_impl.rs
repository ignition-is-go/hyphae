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

use std::{hash::Hash, marker::PhantomData};

use super::{MapDiffSink, MapQuery, MapQueryInstall};
use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    nested_map::NestedMap,
    subscription::SubscriptionGuard,
    traits::{CellValue, reactive_map::ReactiveMap},
};

impl<M> MapQueryInstall<M::Key, M::Value> for M
where
    M: ReactiveMap + Clone,
    M::Key: CellValue + Hash + Eq,
    M::Value: CellValue,
{
    fn install(self, sink: MapDiffSink<M::Key, M::Value>) -> Vec<SubscriptionGuard> {
        // `subscribe_diffs_reactive` borrows `&self`, so we need `self` to
        // outlive the call. We also need to keep the source alive across the
        // subscription lifetime so the underlying inner map doesn't drop
        // while the callback is registered. Cheap clone (`Arc` bump for
        // CellMap; same for BoundedInput) lets us do both: keep one copy
        // for the subscribe call, capture the other into the closure.
        let keepalive = self.clone();
        let guard = self.subscribe_diffs_reactive(move |diff| {
            // Hold `keepalive` across calls. The reference is otherwise
            // unused — its purpose is purely to extend the source's
            // lifetime to match the subscription's.
            let _ = &keepalive;
            sink(diff);
        });
        vec![guard]
    }
}

#[allow(private_bounds)]
impl<K, V, M> MapQuery<K, V> for CellMap<K, V, M>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
    M: Clone + Send + Sync + 'static,
{
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
impl<PK, K, V> MapQuery<K, V> for NestedMap<PK, K, V>
where
    PK: CellValue + Hash + Eq,
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    // Inherits the default materialize. A NestedMap is not a CellMap; it
    // owns its own diff-stream/state and there is no immutable variant to
    // short-circuit to, so the default allocate-and-forward strategy is
    // correct.
}
