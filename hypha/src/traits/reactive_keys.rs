//! Reactive key-set abstraction.
//!
//! `ReactiveKeys` is the minimal interface a collection needs to participate as
//! the parent side of a join: "tell me your keys, and notify me when they
//! change."  Both [`CellMap`] and [`NestedMap`] implement it, so join logic is
//! written once and works with either.
//!
//! A semi-join's output is itself `ReactiveKeys`, which means it can feed
//! directly into the next join — enabling multi-step composition without ever
//! mentioning a concrete map type.

use std::hash::Hash;

use crate::{
    subscription::SubscriptionGuard,
    traits::CellValue,
};

/// Notification for key-set membership changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyChange<K> {
    /// Initial snapshot: the full current key set. Replaces any previous state.
    Initial(Vec<K>),
    /// A key was added to the set.
    Added(K),
    /// A key was removed from the set.
    Removed(K),
    /// Multiple key changes emitted as a single atomic notification.
    Batch(Vec<KeyChange<K>>),
}

/// A reactive set of keys that notifies subscribers when membership changes.
///
/// This is the *minimum* a parent collection needs to expose for a join to
/// function — no values, just key presence and change notifications.
///
/// # Implementors
///
/// | Type | `Key` |
/// |---|---|
/// | `CellMap<K, V>` | `K` |
/// | `NestedMap<PK, K, V>` | `K` |
/// | semi-join output | parent `K` (filtered) |
/// | anti-join output | parent `K` (filtered) |
pub trait ReactiveKeys: Send + Sync + 'static {
    type Key: Hash + Eq + CellValue;

    /// Snapshot of all current keys. Non-reactive, point-in-time.
    fn key_set(&self) -> Vec<Self::Key>;

    /// Does `key` currently exist? Non-reactive, point-in-time.
    fn contains_key(&self, key: &Self::Key) -> bool;

    /// Subscribe to key-set changes.
    ///
    /// The callback receives an initial `KeyChange::Reset` with all current
    /// keys, then `Added`/`Removed` events as they occur.
    fn subscribe_keys(
        &self,
        cb: impl Fn(&KeyChange<Self::Key>) + Send + Sync + 'static,
    ) -> SubscriptionGuard;
}
