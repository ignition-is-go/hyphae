//! Uncompiled reactive map operation chains.
//!
//! A [`MapQuery`] is a recipe for a reactive map computation — a chain of
//! pure operators (joins, projections, selections) that has not yet been
//! materialized into a [`CellMap`]. Map queries deliberately do not implement
//! `subscribe`: to observe output you must call [`MapQuery::materialize`],
//! which installs ONE subscription per root source and returns a
//! subscribable cell map.
//!
//! This design makes the memoization boundary explicit. Today, chaining map
//! operators (`inner_join`, `left_join`, `project`, ...) creates an
//! intermediate [`CellMap`] per stage — each with its own diff cell and
//! per-key cells. By moving these operators onto `MapQuery`, the cost of an
//! intermediate map is paid only when the caller explicitly asks for one
//! with `.materialize()`.

use std::{hash::Hash, marker::PhantomData, sync::Arc};

use crate::{
    cell::{CellImmutable, CellMutable},
    cell_map::{CellMap, MapDiff},
    subscription::SubscriptionGuard,
    traits::CellValue,
};

pub(crate) mod reactive_map_impl;

/// Boxed diff sink shape used by every [`MapQueryInstall::install`] hop.
///
/// A sink consumes diffs from an upstream stage and applies them to a
/// downstream stage's state (or, at the materialize boundary, to the output
/// [`CellMap`]). Sinks are `Arc`-cloneable so a stage can fan out to multiple
/// downstream consumers if needed.
pub(crate) type MapDiffSink<K, V> = Arc<dyn Fn(&MapDiff<K, V>) + Send + Sync>;

/// Crate-private installer hook used by [`MapQuery::materialize`].
///
/// `install` consumes the plan node, subscribes upstream sources, and pipes
/// the diff stream to the provided sink. Returns guards owning upstream
/// subscriptions and any per-stage state that must outlive the materialized
/// output cell map.
///
/// This is separate from [`MapQuery`] so that the public trait stays minimal
/// and cannot be accidentally used to subscribe without materializing.
pub(crate) trait MapQueryInstall<K, V>: Sized + Send + Sync + 'static
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    /// Install this plan's diff propagation into `sink`, returning guards
    /// that own the upstream subscriptions.
    ///
    /// Consumes `self`: a plan can only be materialized once, and its
    /// owned source(s) need to move into the resulting subscription
    /// closures so chained plans compose without cloning.
    fn install(self, sink: MapDiffSink<K, V>) -> Vec<SubscriptionGuard>;
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
/// The `MapQueryInstall<K, V>` supertrait is `pub(crate)`, which seals
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
pub trait MapQuery<K, V>: MapQueryInstall<K, V>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    /// Compile the query into a [`CellMap`] and install root-source
    /// subscriptions running the fused diff-propagation closure.
    ///
    /// This is the only way to observe map-query output. Every subscribe in
    /// the codebase is on a cell map, never on a query — which is the point.
    #[track_caller]
    fn materialize(self) -> CellMap<K, V, CellImmutable> {
        let output = CellMap::<K, V, CellMutable>::new();
        let weak = Arc::downgrade(&output.inner);

        let sink: MapDiffSink<K, V> = Arc::new(move |diff| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let out: CellMap<K, V, CellMutable> = CellMap {
                inner,
                _marker: PhantomData,
            };
            out.apply_diff_ref(diff);
        });

        let guards = self.install(sink);
        for g in guards {
            output.own(g);
        }
        output.lock()
    }
}
