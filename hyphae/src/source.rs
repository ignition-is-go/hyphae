//! [`Source<T>`] — a watchable event channel with no current value.
//!
//! `Source` is to [`Cell`](crate::Cell) as `RxJS`'s `Subject` is to `BehaviorSubject`:
//! it has subscribers and an emit method, but does not store a "current value"
//! and is not [`Gettable`](crate::traits::Gettable).
//!
//! # When to use
//!
//! Use `Source` for pure event/notification channels where:
//! - Subscribers care only about *that* something happened, not the latest
//!   stored value.
//! - The producer fires at high rate and the per-emission `ArcSwap` drop on a
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
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use uuid::Uuid;

use crate::{
    cell::{Cell, CellMutable, Subscriber, SubscriberCallback},
    pipeline::{Definite, Empty, Materialize, Pipeline, PipelineInstall},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, DepNode, Gettable, Mutable, Watchable},
};

/// Inner data of a [`Source`], shared via `Arc`.
pub(crate) struct SourceInner<T> {
    pub(crate) id: Uuid,
    /// Subscriber registry stored as `Mutex<Arc<Vec<…>>>`. Reads (emit, complete)
    /// take the lock briefly to clone the Arc and drop it before invoking
    /// callbacks — user code never runs with this mutex held. Writes
    /// (subscribe/unsubscribe) build the next snapshot under the lock and let
    /// the displaced Arc drop *outside* the lock so cascading drops don't run
    /// with any internal mutex held. See `Cell::subscribers` for the rationale
    /// (avoids arc-swap's per-mutation `Debt::pay_all` slot-walk).
    pub(crate) subscribers: parking_lot::Mutex<Arc<Vec<(Uuid, Arc<Subscriber<T>>)>>>,
    /// Owned subscription guards (e.g. upstream timers feeding this source).
    pub(crate) owned: Mutex<FxHashMap<Uuid, SubscriptionGuard>>,
    /// Whether this source has completed (no more values will be emitted).
    pub(crate) completed: AtomicBool,
    /// Events waiting for this source's turn in the settle in progress.
    pub(crate) pending: Mutex<Vec<Signal<T>>>,
    /// True while this source is in the settle frontier.
    pub(crate) queued: AtomicBool,
    /// The settle in which this source last fanned out.
    pub(crate) fired_run: std::sync::atomic::AtomicU64,
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
    #[must_use]
    pub fn upgrade(&self) -> Option<Source<T>> {
        self.inner.upgrade().map(|inner| Source {
            inner,
            _marker: PhantomData,
        })
    }
}

impl<T> Clone for WeakSource<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T> Clone for Source<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _marker: PhantomData,
        }
    }
}

impl<T: CellValue> Source<T> {
    /// Create a new source with no subscribers.
    #[track_caller]
    #[must_use]
    pub fn new() -> Self {
        let inner = Arc::new(SourceInner {
            id: Uuid::new_v4(),
            subscribers: parking_lot::Mutex::new(Arc::new(Vec::new())),
            owned: Mutex::new(FxHashMap::default()),
            completed: AtomicBool::new(false),
            pending: Mutex::new(Vec::new()),
            queued: AtomicBool::new(false),
            fired_run: std::sync::atomic::AtomicU64::new(0),
            caller: Location::caller(),
        });
        Self {
            inner,
            _marker: PhantomData,
        }
    }

    /// Create a weak reference to this source.
    #[must_use]
    pub fn downgrade(&self) -> WeakSource<T> {
        WeakSource {
            inner: Arc::downgrade(&self.inner),
            _marker: PhantomData,
        }
    }

    /// Take ownership of a subscription guard, dropping it when this source is dropped.
    pub fn own(&self, guard: SubscriptionGuard) {
        let previous = self.inner.owned.lock().insert(Uuid::new_v4(), guard);
        drop(previous);
    }

    /// Returns true if this source has completed.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.inner.completed.load(Ordering::SeqCst)
    }

    /// Builder-style name attachment, kept for API compatibility with
    /// [`Cell::with_name`](crate::Cell::with_name). `Source` carries no name
    /// field, so this is a no-op.
    #[must_use]
    pub fn with_name(self, _name: impl Into<std::sync::Arc<str>>) -> Self {
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
        let sub = Arc::new(Subscriber::new_live(cb));
        // Swap-and-defer: see Cell::subscribe.
        let old_snapshot = {
            let mut guard = self.inner.subscribers.lock();
            let mut next: Vec<(Uuid, Arc<Subscriber<T>>)> = (**guard).clone();
            next.push((id, sub));
            std::mem::replace(&mut *guard, Arc::new(next))
        };
        drop(old_snapshot);

        let source: Arc<dyn DepNode> = Arc::new(self.clone());
        let inner = self.inner.clone();
        SubscriptionGuard::new(id, source, move || {
            let old_snapshot = {
                let mut guard = inner.subscribers.lock();
                let prev_len = guard.len();
                let next: Vec<(Uuid, Arc<Subscriber<T>>)> = (**guard)
                    .iter()
                    .filter(|(i, _)| *i != id)
                    .cloned()
                    .collect();
                if next.len() == prev_len {
                    None
                } else {
                    Some(std::mem::replace(&mut *guard, Arc::new(next)))
                }
            };
            drop(old_snapshot);
        })
    }

    /// Emit a value to all subscribers. The value is not stored: there is no
    /// `.get()` on `Source`.
    ///
    /// Each emit is its own event. Two emits on one source never share a
    /// settle, so a subscriber sees every one, in order.
    pub fn emit(&self, value: T) {
        self.inner.write(Signal::Value(Arc::new(value)));
    }

    /// Mark this source as completed. Later [`emit`](Self::emit) calls are
    /// dropped.
    pub fn complete(&self) {
        self.inner.write(Signal::Complete);
    }
}

impl<T: CellValue> SourceInner<T> {
    fn write(self: &Arc<Self>, signal: Signal<T>) {
        if self.completed.load(Ordering::SeqCst) {
            return;
        }
        // A source has height 0, so a write made during a settle never joins
        // it. Every emit goes through the input queue under the source's key.
        let (high, low) = self.id.as_u64_pair();
        let key = high ^ low;
        crate::settle::submit_write(
            crate::settle::SettleKey::Exclusive(key),
            Box::new(SourceWrite {
                source: Arc::downgrade(self),
                signal,
            }),
        );
    }
}

struct SourceWrite<T> {
    source: Weak<SourceInner<T>>,
    signal: Signal<T>,
}

impl<T: CellValue> crate::settle::Write for SourceWrite<T> {
    fn apply(self: Box<Self>) {
        let Some(source) = self.source.upgrade() else {
            return;
        };
        source.pending.lock().push(self.signal);
        let node: Arc<dyn crate::settle::Node> = source;
        crate::settle::schedule(Arc::downgrade(&node), 0);
    }
}

impl<T: CellValue> crate::settle::Node for SourceInner<T> {
    fn node_id(&self) -> Uuid {
        self.id
    }

    fn node_height(&self) -> u64 {
        0
    }

    fn claim(&self) -> bool {
        !self.queued.swap(true, Ordering::AcqRel)
    }

    fn fire(&self) {
        self.queued.store(false, Ordering::Release);
        let pending = std::mem::take(&mut *self.pending.lock());
        for signal in pending {
            let terminal = !matches!(signal, Signal::Value(_));
            let already_complete = if terminal {
                self.completed.swap(true, Ordering::SeqCst)
            } else {
                self.completed.load(Ordering::SeqCst)
            };
            if already_complete {
                break;
            }
            crate::settle::record_fire(&self.fired_run);
            // Callbacks run with the registry lock released.
            let subscribers = Arc::clone(&*self.subscribers.lock());
            for (_id, subscriber) in subscribers.iter() {
                (subscriber.callback)(&signal);
            }
        }
    }

    fn bump_height_epoch(&self) {}

    fn node_dependents(&self) -> Vec<Weak<dyn crate::settle::Node>> {
        Vec::new()
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
// A source has no seed: Empty materialization installs it directly and starts
// the resulting cell at `None`.
//
// Materialization uses `Materialize<T, Empty>`, producing a
// `Cell<Option<T>, CellImmutable>` that starts as `None` and transitions to
// `Some(value)` on the first `emit`.

impl<T: CellValue> PipelineInstall<T> for Source<T> {
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        self.subscribe(move |signal| callback(signal))
    }
}

#[allow(private_bounds)]
impl<T: CellValue> Pipeline<T, Empty> for Source<T> {}

/// Extension trait: sample a [`Watchable`] cell using a [`Source`] as the
/// notifier.
///
/// Mirrors `Watchable::sample(&pipeline_notifier)` but accepts a `Source<U>`.
/// The returned pipeline surface keeps the materialization boundary explicit
/// even though the current implementation uses a cell internally.
pub trait SampleOnSourceExt<T: CellValue>: Watchable<T> + Gettable<T> {
    /// Build a pipeline that re-emits the current value of `self` whenever
    /// `notifier` emits, with lifecycle owned by the resulting cell.
    #[track_caller]
    fn sample_on<U>(&self, notifier: &Source<U>) -> impl Materialize<T, Definite> + use<Self, T, U>
    where
        U: CellValue,
    {
        let source = self.clone();
        let cell = Cell::<T, CellMutable>::new(source.get());
        // Weak, not a strong clone: `cell.own(guard)` below hands the cell
        // ownership of the subscription. Capturing the cell strongly here
        // would create an unbreakable cell -> guard -> callback -> cell cycle.
        let weak_cell = cell.downgrade();
        let guard = notifier.subscribe(move |signal| {
            if let Signal::Value(_) = signal
                && let Some(cell) = weak_cell.upgrade()
            {
                cell.set(source.get());
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
        Arc::clone(&*self.inner.subscribers.lock()).len()
    }

    fn owned_count(&self) -> usize {
        self.inner.owned.lock().len()
    }
}

impl<T> Drop for SourceInner<T> {
    fn drop(&mut self) {}
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering};

    use super::*;
    use crate::Materialize;

    #[test]
    fn owned_guards_are_released_when_source_drops() {
        use crate::{Watchable, traits::DepNode};
        let upstream = Cell::new(0u64);
        let source = Source::<u64>::new();
        let weak = source.downgrade();
        source.own(upstream.subscribe(|_| {}));
        assert_eq!(source.owned_count(), 1);
        assert_eq!(upstream.subscriber_count(), 1);
        drop(source);
        assert!(weak.upgrade().is_none());
        assert_eq!(upstream.subscriber_count(), 0);
    }

    /// `sample_on` used to capture a strong clone of its own output cell in the
    /// notifier's subscriber closure while also owning the guard, forming an
    /// unbreakable cycle: the cell could never drop (rship lv-df48).
    #[test]
    fn sample_on_cell_is_freed_when_dropped() {
        let value = Cell::new(7i32);
        let notifier: Source<()> = Source::new();

        let sampled = value.sample_on(&notifier).materialize();
        let weak = sampled.downgrade();

        value.set(8);
        notifier.emit(());
        assert_eq!(sampled.get(), 8, "sampling must still work");

        drop(sampled);
        assert!(
            weak.upgrade().is_none(),
            "sample_on output cell outlived its last owner — Arc cycle is back"
        );
    }

    /// The CPU-leak half of the same bug: once the sampled cell is gone the
    /// subscription must be torn down, not left sampling forever.
    #[test]
    fn sample_on_unsubscribes_when_cell_dropped() {
        let value = Cell::new(0i32);
        let notifier: Source<()> = Source::new();

        let sampled = value.sample_on(&notifier).materialize();
        assert_eq!(notifier.subscriber_count(), 1);

        drop(sampled);
        assert_eq!(
            notifier.subscriber_count(),
            0,
            "dropped sample_on cell left its subscription live on the notifier"
        );
    }

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
            Signal::Error(_) => {}
        });

        src.emit(1);
        src.complete();
        src.emit(99); // dropped
        assert_eq!(counter.load(Ordering::SeqCst), 1 + 1000);
    }
}
