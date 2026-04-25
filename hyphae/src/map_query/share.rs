//! Clone-able multicast handle that subscribes a [`MapQuery`] upstream lazily
//! on the first downstream install, then fans diffs out to N consumers.
//!
//! Mirrors [`crate::pipeline::SharedPipeline`] for value pipelines, but the
//! signal type is [`MapDiff`] and the upstream may produce multiple
//! [`SubscriptionGuard`]s (one per root source).
//!
//! # Hot-path emission cost
//!
//! Each upstream diff costs:
//!  * one Mutex-acquire to apply the diff to a small in-memory snapshot
//!    (needed so late-binding subscribers can be replayed an Initial), and
//!  * one [`ArcSwap::load`] to pull the subscriber list, plus N callback
//!    invocations to fan the diff out.
//!
//! No mutex on the subscriber list during fanout, no per-emission allocation
//! beyond what the in-place state mutation already needs. Compared to
//! materialize-then-clone-cell — where each shared consumer would walk the
//! full per-key cell + diffs_cell pipeline — this saves the per-share-point
//! `CellMap` allocation and most of its wiring.
//!
//! # Lifecycle
//!
//! On the first downstream install, the share point consumes the wrapped
//! upstream plan and installs once. The returned upstream guards are kept
//! until the last shared subscriber drops, at which point they are released.
//! The upstream plan was consumed on first install, so a fully drained
//! `SharedMapQuery` does not reactivate — hold the handle (or one
//! materialized leaf) for as long as you want the shared work to run.

use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex},
};

use arc_swap::ArcSwap;
use uuid::Uuid;

use crate::{
    cell_map::MapDiff,
    map_query::{MapDiffSink, MapQuery, MapQueryInstall},
    subscription::SubscriptionGuard,
    traits::CellValue,
};

type DiffSubscriber<K, V> = Arc<dyn Fn(&MapDiff<K, V>) + Send + Sync>;

/// Type-erased one-shot installer for the wrapped upstream plan. `MapQuery`'s
/// `install` consumes `self`, so we wrap a one-shot `FnOnce` in a slot and
/// take it on the first downstream install.
type UpstreamInstall<K, V> =
    Box<dyn FnOnce(MapDiffSink<K, V>) -> Vec<SubscriptionGuard> + Send + Sync>;

pub(crate) struct SharedMapQueryInner<K, V>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    /// One-shot upstream installer. Taken on the first downstream install;
    /// `None` thereafter. Drained share points do not reactivate.
    upstream: Mutex<Option<UpstreamInstall<K, V>>>,
    /// Held subscriptions on the upstream plan once installed. Released when
    /// the last shared subscriber drops.
    upstream_guards: Mutex<Vec<SubscriptionGuard>>,
    /// In-memory snapshot of the joined / projected state. Applied on every
    /// diff so a late-binding subscriber can be replayed an Initial without
    /// re-running the upstream plan.
    state: Mutex<HashMap<K, V>>,
    /// Lock-free subscriber registry. Read on every emission (`.load()`),
    /// rebuilt on subscribe / unsubscribe under the writer mutex.
    subscribers: ArcSwap<Vec<(Uuid, DiffSubscriber<K, V>)>>,
    /// Serializes mutation of `subscribers`. Emission readers do not acquire.
    subs_writer: Mutex<()>,
}

impl<K, V> SharedMapQueryInner<K, V>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    fn add_subscriber(&self, id: Uuid, cb: DiffSubscriber<K, V>) {
        let _w = self.subs_writer.lock().expect("share subs_writer poisoned");
        let current = self.subscribers.load();
        let mut next = (**current).clone();
        next.push((id, cb));
        self.subscribers.store(Arc::new(next));
    }

    /// Returns the remaining subscriber count after removal.
    fn remove_subscriber(&self, id: Uuid) -> usize {
        let _w = self.subs_writer.lock().expect("share subs_writer poisoned");
        let current = self.subscribers.load();
        let mut next: Vec<(Uuid, DiffSubscriber<K, V>)> =
            (**current).iter().filter(|(i, _)| *i != id).cloned().collect();
        let remaining = next.len();
        next.shrink_to_fit();
        self.subscribers.store(Arc::new(next));
        remaining
    }

    /// Apply a diff to the in-memory snapshot. Called from the fanout closure
    /// before downstream callbacks fire so that any concurrent late-binding
    /// install sees a consistent state.
    fn apply_diff(&self, diff: &MapDiff<K, V>) {
        let mut state = self.state.lock().expect("share state poisoned");
        Self::apply_diff_into(&mut state, diff);
    }

    fn apply_diff_into(state: &mut HashMap<K, V>, diff: &MapDiff<K, V>) {
        match diff {
            MapDiff::Initial { entries } => {
                state.clear();
                state.reserve(entries.len());
                for (k, v) in entries {
                    state.insert(k.clone(), v.clone());
                }
            }
            MapDiff::Insert { key, value } => {
                state.insert(key.clone(), value.clone());
            }
            MapDiff::Remove { key, .. } => {
                state.remove(key);
            }
            MapDiff::Update { key, new_value, .. } => {
                state.insert(key.clone(), new_value.clone());
            }
            MapDiff::Batch { changes } => {
                for c in changes {
                    Self::apply_diff_into(state, c);
                }
            }
        }
    }

    fn snapshot_initial(&self) -> MapDiff<K, V> {
        let state = self.state.lock().expect("share state poisoned");
        let entries: Vec<(K, V)> =
            state.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        MapDiff::Initial { entries }
    }
}

/// Clone-able multicast handle for fanning a [`MapQuery`] out to many
/// consumers without an intermediate [`crate::CellMap`].
///
/// Cloning is an `Arc` bump and does not subscribe upstream. Subscription
/// happens lazily on the first downstream `materialize()` (or other install).
/// Once installed, the upstream subscription stays live until the last
/// shared subscriber drops.
pub struct SharedMapQuery<K, V>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    inner: Arc<SharedMapQueryInner<K, V>>,
}

impl<K, V> Clone for SharedMapQuery<K, V>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K, V> SharedMapQuery<K, V>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    /// Wrap a map query in a shared, multicast handle.
    ///
    /// Prefer the [`MapQueryShareExt::share`] extension method — it reads as
    /// `query.share()` at the call site.
    pub fn new<Q: MapQuery<K, V>>(q: Q) -> Self {
        let upstream: UpstreamInstall<K, V> = Box::new(move |sink| q.install(sink));
        Self {
            inner: Arc::new(SharedMapQueryInner {
                upstream: Mutex::new(Some(upstream)),
                upstream_guards: Mutex::new(Vec::new()),
                state: Mutex::new(HashMap::new()),
                subscribers: ArcSwap::from_pointee(Vec::new()),
                subs_writer: Mutex::new(()),
            }),
        }
    }
}

impl<K, V> MapQueryInstall<K, V> for SharedMapQuery<K, V>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    fn install(self, sink: MapDiffSink<K, V>) -> Vec<SubscriptionGuard> {
        let id = Uuid::new_v4();

        // Decide whether this is the first install. If the upstream slot is
        // still populated, we will:
        //   1. Register `sink` first so the synchronous Initial diff emitted
        //      by upstream during install reaches it.
        //   2. Take and run the upstream installer.
        //   3. Stash the returned guards.
        //
        // If the upstream has already been installed by an earlier consumer,
        // we instead:
        //   1. Register `sink`.
        //   2. Synthesize an Initial from our state snapshot and deliver it
        //      to the new `sink` so it sees a coherent starting state.
        let upstream_take = {
            let mut slot = self
                .inner
                .upstream
                .lock()
                .expect("share upstream poisoned");
            slot.take()
        };

        if let Some(install_fn) = upstream_take {
            self.inner.add_subscriber(id, sink);
            let weak = Arc::downgrade(&self.inner);
            let fanout: MapDiffSink<K, V> = Arc::new(move |diff: &MapDiff<K, V>| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                // 1. Update internal state under its own Mutex.
                inner.apply_diff(diff);
                // 2. Lock-free fanout to the subscriber snapshot.
                let subs = inner.subscribers.load();
                for (_, cb) in subs.iter() {
                    cb(diff);
                }
            });
            let guards = install_fn(fanout);
            let mut slot = self
                .inner
                .upstream_guards
                .lock()
                .expect("share upstream_guards poisoned");
            slot.extend(guards);
        } else {
            // Replay current state to the late-binding subscriber, then
            // register so it picks up subsequent diffs. Order matters: if we
            // registered first, an upstream emission landing between
            // registration and our manual Initial would arrive before the
            // Initial — out-of-order.
            //
            // Synthesize the Initial under the state lock so a concurrent
            // upstream emission cannot mutate state while we read it.
            let initial = self.inner.snapshot_initial();
            sink(&initial);
            self.inner.add_subscriber(id, sink);
        }

        // Guard whose Drop removes this subscriber and, if last, releases all
        // upstream guards.
        let weak = Arc::downgrade(&self.inner);
        vec![SubscriptionGuard::from_callback(move || {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let remaining = inner.remove_subscriber(id);
            if remaining == 0 {
                // Drain upstream guards outside any other lock: dropping a
                // guard may itself acquire locks.
                let drained: Vec<SubscriptionGuard> = {
                    let mut slot = inner
                        .upstream_guards
                        .lock()
                        .expect("share upstream_guards poisoned");
                    std::mem::take(&mut *slot)
                };
                drop(drained);
            }
        })]
    }
}

#[allow(private_bounds)]
impl<K, V> MapQuery<K, V> for SharedMapQuery<K, V>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    // Default `materialize` is correct: it allocates a CellMap that subscribes
    // through `install()` above. Each materialized leaf adds one fan-out
    // subscriber; the share point's upstream plan installs exactly once.
}

/// Extension trait that adds [`share`](MapQueryShareExt::share) to any
/// [`MapQuery`].
///
/// `share()` consumes the query and returns a Clone-able [`SharedMapQuery`]
/// handle. Each clone of the handle, when materialized (or otherwise
/// installed), adds one fan-out subscriber but does NOT add another upstream
/// subscription — the share point installs upstream exactly once.
pub trait MapQueryShareExt<K, V>: MapQuery<K, V>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    /// Convert this map query into a Clone-able multicast handle.
    fn share(self) -> SharedMapQuery<K, V> {
        SharedMapQuery::new(self)
    }
}

impl<K, V, Q> MapQueryShareExt<K, V> for Q
where
    K: CellValue + Hash + Eq,
    V: CellValue,
    Q: MapQuery<K, V>,
{
}
