//! Blanket [`MapQuery`] implementation for any [`ReactiveMap`].
//!
//! Every reactive-map source — [`CellMap`], future `NestedMap`, etc. —
//! implements [`MapQuery`] via this blanket, so `cell_map.materialize()` is
//! a no-op identity (returns a locked clone) and any reactive map can be
//! passed as input to a query operator.
//!
//! Concrete plan-node structs (`InnerJoinPlan`, ...) are NOT `ReactiveMap`
//! and provide their own `MapQuery` impls.

use std::hash::Hash;

use super::{MapDiffSink, MapQuery, MapQueryInstall};
use crate::{
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
impl<M> MapQuery<M::Key, M::Value> for M
where
    M: ReactiveMap + Clone,
    M::Key: CellValue + Hash + Eq,
    M::Value: CellValue,
{
}
