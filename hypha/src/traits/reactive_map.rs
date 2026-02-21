//! Reactive map abstraction — extends [`ReactiveKeys`] with values.
//!
//! `ReactiveMap` is the full interface a collection exposes for joins: keys,
//! values, point-in-time lookups, and diff-level subscriptions.  Any type that
//! implements `ReactiveMap` can appear on either side of an `inner_join`,
//! `left_join`, `left_semi_join`, etc.
//!
//! Today only [`CellMap`] implements this.  When [`NestedMap`] lands, it will
//! implement the same trait, and the existing join logic will work with it
//! unchanged.

use crate::{
    cell_map::MapDiff,
    subscription::SubscriptionGuard,
    traits::CellValue,
};

use super::reactive_keys::ReactiveKeys;

/// A reactive key-value map that notifies subscribers of changes via
/// [`MapDiff`].
///
/// This is the interface the join runtime operates against. By programming
/// against `ReactiveMap` rather than `CellMap` directly, joins compose with
/// any map-like source — `CellMap`, `NestedMap`, or a derived map from a
/// previous join.
pub trait ReactiveMap: ReactiveKeys {
    type Value: CellValue;

    /// Point-in-time value lookup by key.
    fn get_value(&self, key: &Self::Key) -> Option<Self::Value>;

    /// Point-in-time snapshot of all entries.
    fn snapshot(&self) -> Vec<(Self::Key, Self::Value)>;

    /// Subscribe to diffs with an initial snapshot.
    ///
    /// The callback is first called with `MapDiff::Initial` containing all
    /// current entries, then called with subsequent diffs as the map changes.
    fn subscribe_diffs_reactive(
        &self,
        cb: impl Fn(&MapDiff<Self::Key, Self::Value>) + Send + Sync + 'static,
    ) -> SubscriptionGuard;
}
