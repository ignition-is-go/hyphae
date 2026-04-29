//! [`Source<T>`] — a watchable event channel with no current value.
//!
//! `Source` is to [`Cell`](crate::Cell) as RxJS's `Subject` is to `BehaviorSubject`:
//! it has subscribers and an emit method, but does not store a "current value"
//! and is not [`Gettable`](crate::traits::Gettable).
//!
//! # When to use
//!
//! Use `Source` for pure event/notification channels where:
//! - Subscribers care only about *that* something happened, not the latest
//!   stored value.
//! - The producer fires at high rate and the per-emission ArcSwap drop on a
//!   [`Cell`]'s value field is pure overhead.
//!
//! Concrete examples: interval ticks, clock pulses, lifecycle events.
//!
//! # Tradeoff vs. [`Cell`]
//!
//! `Source` deliberately does **not** implement [`Gettable`] or
//! [`Watchable`](crate::traits::Watchable) (which requires `Gettable` as a
//! supertrait). Operators that need a current value to bootstrap from — `join`,
//! `combine_latest`, `with_latest_from`, and `.sample` *as the source side* —
//! cannot accept a `Source` and the type system rejects it at compile time.
//!
//! `Source` implements [`Watchable`]'s subscribe/lifecycle surface directly via
//! its own inherent `subscribe`/`emit`/`complete` methods.

use std::{
    marker::PhantomData,
    panic::Location,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use arc_swap::ArcSwap;
use dashmap::DashMap;
use uuid::Uuid;

use crate::{
    cell::{Cell, CellImmutable, CellMutable, Subscriber, SubscriberCallback},
    pipeline::{Empty, MaterializeEmpty, Pipeline, PipelineInstall},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, DepNode, Gettable, Mutable, Watchable},
};

/// Inner data of a [`Source`], shared via `Arc`.
pub(crate) struct SourceInner<T> {
    pub(crate) id: Uuid,
    /// Lock-free subscriber registry. Read on every emit (`.load()`), rebuilt
    /// on subscribe / unsubscribe under `subscribers_writer`.
    pub(crate) subscribers: ArcSwap<Vec<(Uuid, Arc<Subscriber<T>>)>>,
    /// Serializes mutation of `subscribers` so subscribe / unsubscribe build
    /// the next snapshot from the current one without lost updates.
    pub(crate) subscribers_writer: Mutex<()>,
    /// Owned subscription guards (e.g. upstream timers feeding this source).
    pub(crate) owned: DashMap<Uuid, SubscriptionGuard>,
    /// Whether this source has completed (no more values will be emitted).
    pub(crate) completed: AtomicBool,
    /// Source location where this source was created (`#[track_caller]`).
    #[allow(dead_code)]
    pub(crate) caller: &'static Location<'static>,
}

/// A watchable event channel with no current value.
///
/// Subscribers receive [`Signal::Value`] when [`emit`](Source::emit) is called
/// and [`Signal::Complete`] when [`complete`](Source::complete) is called.
/// There is no `get()` and no last-value storage — this is the entire point of
/// `Source` vs. [`Cell`](crate::Cell).
pub struct Source<T> {
    pub(crate) inner: Arc<SourceInner<T>>,
    _marker: PhantomData<T>,
}

/// A weak reference to a [`Source`] that doesn't prevent it from being dropped.
pub struct WeakSource<T> {
    inner: Weak<SourceInner<T>>,
    _marker: PhantomData<T>,
}

impl<T> WeakSource<T> {
    /// Try to upgrade to a strong [`Source`] reference. Returns `None` if the
    /// underlying source has been dropped.
    pub fn upgrade(&self) -> Option<Source<T>> {
        self.inner.upgrade().map(|inner| Source {
            inner,
            _marker: PhantomData,
        })
    }
}

impl<T> Clone for WeakSource<T> {
    fn clone(&self) -> Self {
        WeakSource {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T> Clone for Source<T> {
    fn clone(&self) -> Self {
        Source {
            inner: Arc::clone(&self.inner),
            _marker: PhantomData,
        }
    }
}

impl<T: CellValue> Source<T> {
    /// Create a new source with no subscribers.
    #[track_caller]
    pub fn new() -> Self {
        let inner = Arc::new(SourceInner {
            id: Uuid::new_v4(),
            subscribers: ArcSwap::from_pointee(Vec::new()),
            subscribers_writer: Mutex::new(()),
            owned: DashMap::new(),
            completed: AtomicBool::new(false),
            caller: Location::caller(),
        });
        #[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
        crate::registry::registry().register(inner.id, Arc::downgrade(&inner) as Weak<dyn DepNode>);
        #[cfg(feature = "trace")]
        crate::tracing::register_cell(inner.id, Some(Location::caller().to_string()));
        Self {
            inner,
            _marker: PhantomData,
        }
    }

    /// Create a weak reference to this source.
    pub fn downgrade(&self) -> WeakSource<T> {
        WeakSource {
            inner: Arc::downgrade(&self.inner),
            _marker: PhantomData,
        }
    }

    /// Take ownership of a subscription guard, dropping it when this source is dropped.
    pub fn own(&self, guard: SubscriptionGuard) {
        #[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
        crate::registry::registry().mark_owned(guard.source().id(), self.inner.id);
        self.inner.owned.insert(Uuid::new_v4(), guard);
    }

    /// Returns true if this source has completed.
    pub fn is_complete(&self) -> bool {
        self.inner.completed.load(Ordering::SeqCst)
    }

    /// Builder-style name attachment for tracing/inspector. No-op when the
    /// inspector / trace features are off.
    #[allow(unused_variables)]
    pub fn with_name(self, name: impl Into<std::sync::Arc<str>>) -> Self {
        #[cfg(feature = "trace")]
        crate::tracing::update_name(self.inner.id, name.into().to_string());
        self
    }

    /// Subscribe to all signals. Returns a guard that unsubscribes when dropped.
    ///
    /// Unlike [`Cell::subscribe`](crate::Cell), `Source::subscribe` does **not**
    /// invoke the callback synchronously with a current value — there is no
    /// current value. The callback fires only on subsequent [`emit`](Self::emit)
    /// or [`complete`](Self::complete) calls.
    #[track_caller]
    pub fn subscribe(
        &self,
        callback: impl Fn(&Signal<T>) + Send + Sync + 'static,
    ) -> SubscriptionGuard {
        let id = Uuid::new_v4();
        let cb: SubscriberCallback<T> = Arc::new(callback);
        let sub = Arc::new(Subscriber { callback: cb });
        {
            let _w = self
                .inner
                .subscribers_writer
                .lock()
                .expect("source subscribers_writer poisoned");
            let current = self.inner.subscribers.load();
            let mut next = (**current).clone();
            next.push((id, sub));
            self.inner.subscribers.store(Arc::new(next));
        }

        let source: Arc<dyn DepNode> = Arc::new(self.clone());
        let inner = self.inner.clone();
        SubscriptionGuard::new(id, source, move || {
            let _w = inner
                .subscribers_writer
                .lock()
                .expect("source subscribers_writer poisoned");
            let current = inner.subscribers.load();
            let prev_len = current.len();
            let next: Vec<(Uuid, Arc<Subscriber<T>>)> = (**current)
                .iter()
                .filter(|(i, _)| *i != id)
                .cloned()
                .collect();
            if next.len() != prev_len {
                inner.subscribers.store(Arc::new(next));
            }
        })
    }

    /// Emit a value to all subscribers. Does **not** store the value — there
    /// is no `.get()` on `Source`.
    ///
    /// This is the hot path. Compared to [`Cell::set`](crate::Cell::set) it
    /// skips the `value: ArcSwap<T>` store and the associated `Debt::pay_all`
    /// drop on the previous value. Useful when emission rate is high
    /// (timers, clocks, ticks).
    pub fn emit(&self, value: T) {
        if self.inner.completed.load(Ordering::SeqCst) {
            return;
        }
        let arc_value = Arc::new(value);
        let signal = Signal::Value(arc_value);

        let subs = self.inner.subscribers.load();
        // Subscriber callbacks must not panic — same contract as Cell::notify.
        for (_id, sub) in subs.iter() {
            (sub.callback)(&signal);
        }
    }

    /// Mark this source as completed. Subsequent [`emit`](Self::emit) calls
    /// are dropped silently.
    pub fn complete(&self) {
        if self.inner.completed.swap(true, Ordering::SeqCst) {
            return;
        }
        let signal: Signal<T> = Signal::Complete;
        let subs = self.inner.subscribers.load();
        for (_id, sub) in subs.iter() {
            (sub.callback)(&signal);
        }
    }
}

impl<T: CellValue> Default for Source<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Pipeline integration
// ─────────────────────────────────────────────────────────────────────────────
//
// `Source<T>` is a `Pipeline<T, Empty>`: operators that don't require a
// definite initial value (`map`, `filter`, `tap`, ...) compose with it.
// `PipelineSeed` is intentionally NOT implemented — operators like `.sample`
// (with Source as the value side), `.join`, `.combine_latest` require a
// current value, and the type system rejects them at compile time.
//
// Materialization goes through `MaterializeEmpty`, producing a
// `Cell<Option<T>, CellImmutable>` that starts as `None` and transitions to
// `Some(value)` on the first `emit`.

impl<T: CellValue> PipelineInstall<T> for Source<T> {
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        self.subscribe(move |signal| callback(signal))
    }
}

#[allow(private_bounds)]
impl<T: CellValue> Pipeline<T, Empty> for Source<T> {}

impl<T: CellValue> MaterializeEmpty<T> for Source<T> {}

/// Extension trait: sample a [`Watchable`] cell using a [`Source`] as the
/// notifier.
///
/// Mirrors `Watchable::sample(&pipeline_notifier)` but accepts a `Source<U>`,
/// which doesn't impl `Pipeline`/`PipelineSeed`. The result is materialized
/// directly into a `Cell<T, CellImmutable>` seeded from the source's current
/// value.
pub trait SampleOnSourceExt<T: CellValue>: Watchable<T> + Gettable<T> {
    /// Build a cell that re-emits the current value of `self` whenever
    /// `notifier` emits. Equivalent to `notifier.subscribe(|_| cell.set(self.get()))`
    /// but with subscription lifecycle owned by the resulting cell.
    #[track_caller]
    fn sample_on<U>(&self, notifier: &Source<U>) -> Cell<T, CellImmutable>
    where
        U: CellValue,
    {
        let source = self.clone();
        let cell = Cell::<T, CellMutable>::new(source.get());
        let cell_for_cb = cell.clone();
        let guard = notifier.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                cell_for_cb.set(source.get());
            }
        });
        cell.own(guard);
        cell.lock()
    }
}

impl<T: CellValue, W: Watchable<T> + Gettable<T>> SampleOnSourceExt<T> for W {}

impl<T: Send + Sync> DepNode for Source<T> {
    fn id(&self) -> Uuid {
        self.inner.id
    }

    fn name(&self) -> Option<String> {
        None
    }

    fn deps(&self) -> Vec<Arc<dyn DepNode>> {
        Vec::new()
    }

    fn subscriber_count(&self) -> usize {
        self.inner.subscribers.load().len()
    }

    fn owned_count(&self) -> usize {
        self.inner.owned.len()
    }
}

#[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
impl<T: Send + Sync> DepNode for SourceInner<T> {
    fn id(&self) -> Uuid {
        self.id
    }

    fn name(&self) -> Option<String> {
        None
    }

    fn deps(&self) -> Vec<Arc<dyn DepNode>> {
        Vec::new()
    }

    fn subscriber_count(&self) -> usize {
        self.subscribers.load().len()
    }

    fn owned_count(&self) -> usize {
        self.owned.len()
    }
}

impl<T> Drop for SourceInner<T> {
    fn drop(&mut self) {
        #[cfg(feature = "trace")]
        crate::tracing::deregister_cell(&self.id);
        #[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
        crate::registry::registry().deregister(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering};

    use super::*;

    #[test]
    fn emit_fires_subscribers() {
        let src: Source<u64> = Source::new();
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let counter_clone = counter.clone();
        let _guard = src.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                counter_clone.fetch_add(**v, Ordering::SeqCst);
            }
        });

        src.emit(1);
        src.emit(2);
        src.emit(3);

        assert_eq!(counter.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn no_initial_emission_on_subscribe() {
        let src: Source<u64> = Source::new();
        src.emit(42); // emitted before any subscriber

        let saw_anything = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_clone = saw_anything.clone();
        let _guard = src.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                saw_clone.store(true, Ordering::SeqCst);
            }
        });

        // Source has no current value — late subscriber sees nothing until next emit.
        assert!(!saw_anything.load(Ordering::SeqCst));

        src.emit(7);
        assert!(saw_anything.load(Ordering::SeqCst));
    }

    #[test]
    fn unsubscribe_via_guard_drop() {
        let src: Source<u64> = Source::new();
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let counter_clone = counter.clone();
        let guard = src.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                counter_clone.fetch_add(**v, Ordering::SeqCst);
            }
        });

        src.emit(5);
        drop(guard);
        src.emit(10);

        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn complete_blocks_further_emits() {
        let src: Source<u64> = Source::new();
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let counter_clone = counter.clone();
        let _guard = src.subscribe(move |signal| match signal {
            Signal::Value(v) => {
                counter_clone.fetch_add(**v, Ordering::SeqCst);
            }
            Signal::Complete => {
                counter_clone.fetch_add(1000, Ordering::SeqCst);
            }
            _ => {}
        });

        src.emit(1);
        src.complete();
        src.emit(99); // dropped
        assert_eq!(counter.load(Ordering::SeqCst), 1 + 1000);
    }
}
