//! Clone-able multicast handle that subscribes upstream lazily on the first
//! downstream install, then fans out to N consumers.
//!
//! [`SharedPipeline`] addresses a wide-fan-out pattern where one expensive
//! pipeline feeds many downstream chains. Without `share()`, a caller would
//! materialize the upstream into a [`Cell`] (one [`ArcSwap`] cache) and clone
//! that cell N times. `SharedPipeline` removes the value-cache `ArcSwap` —
//! emission is `(1 ArcSwap.load) + (N callback invocations)` and nothing else.
//!
//! # Hot-path emission cost
//!
//! On every emission, the share point loads its subscriber list (one
//! [`ArcSwap::load`] — a lock-free Arc bump) and invokes each callback in turn.
//! No mutex, no allocation, no value cache. Compared to `materialize() →
//! clone() × N`, this saves one `ArcSwap.store` per emission per share point.
//!
//! # Lifecycle
//!
//! The share point holds the upstream subscription only while at least one
//! consumer is installed. When the last shared subscriber drops, the upstream
//! guard is released. Re-installing after that point is a no-op: a
//! `SharedPipeline` that has been fully drained will not reactivate. Hold the
//! `SharedPipeline` (or one materialized leaf) for as long as you want the
//! shared work to keep running.

use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use uuid::Uuid;

use crate::{
    pipeline::{Pipeline, PipelineInstall},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable},
};

/// Type-erased upstream handle: holds the original pipeline alive and exposes
/// `get()` + `install()` so [`SharedPipeline`] can defer materialization.
trait UpstreamHandle<T>: Send + Sync + 'static {
    fn get(&self) -> T;
    fn install_upstream(
        &self,
        sink: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard;
}

struct UpstreamWrap<P>(P);

impl<T, P> UpstreamHandle<T> for UpstreamWrap<P>
where
    T: CellValue,
    P: Pipeline<T>,
{
    fn get(&self) -> T {
        self.0.get()
    }
    fn install_upstream(
        &self,
        sink: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
        self.0.install(sink)
    }
}

type Subscriber<T> = Arc<dyn Fn(&Signal<T>) + Send + Sync>;

pub(crate) struct SharedPipelineInner<T: CellValue> {
    /// Original pipeline kept alive so `get()` can recompute and so the first
    /// downstream install can subscribe. Type-erased through `UpstreamHandle`.
    upstream: Arc<dyn UpstreamHandle<T>>,
    /// Held subscription on the upstream once the first downstream consumer
    /// installs. `None` until that point and again after the last downstream
    /// subscriber drops.
    upstream_guard: Mutex<Option<SubscriptionGuard>>,
    /// Lock-free subscriber registry. Read on every emission (`.load()`),
    /// rebuilt on subscribe / unsubscribe under the inner registry mutex.
    subscribers: ArcSwap<Vec<(Uuid, Subscriber<T>)>>,
    /// Serializes mutation of `subscribers` so subscribe / unsubscribe can
    /// build the next snapshot from the current one without lost updates.
    /// Emission readers do not acquire this — they go through `subscribers`
    /// directly.
    subs_writer: Mutex<()>,
}

impl<T: CellValue> SharedPipelineInner<T> {
    fn add_subscriber(&self, id: Uuid, cb: Subscriber<T>) {
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
        let mut next: Vec<(Uuid, Subscriber<T>)> =
            (**current).iter().filter(|(i, _)| *i != id).cloned().collect();
        let remaining = next.len();
        next.shrink_to_fit();
        self.subscribers.store(Arc::new(next));
        remaining
    }
}

/// Clone-able multicast handle for fanning a [`Pipeline`] out to many
/// consumers without an intermediate [`Cell`].
///
/// Cloning a `SharedPipeline` is an `Arc` bump — it does not subscribe
/// upstream. Subscription happens lazily on the first downstream
/// `materialize()` (or other `install()` consumer). Once installed, the
/// upstream subscription stays live until the last shared subscriber drops.
///
/// # Example
///
/// ```
/// use hyphae::{Cell, Gettable, MapExt, Mutable, Pipeline, PipelineShareExt};
///
/// let src = Cell::new(1u64);
/// let shared = src.clone().map(|x| x * 2).share();
///
/// // Cloning the share is cheap — no upstream subscription yet.
/// let m1 = shared.clone().map(|x| x + 1).materialize();
/// let m2 = shared.clone().map(|x| x + 10).materialize();
///
/// src.set(5);
/// assert_eq!(m1.get(), 5 * 2 + 1);
/// assert_eq!(m2.get(), 5 * 2 + 10);
/// ```
pub struct SharedPipeline<T: CellValue> {
    inner: Arc<SharedPipelineInner<T>>,
}

impl<T: CellValue> Clone for SharedPipeline<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: CellValue> SharedPipeline<T> {
    /// Wrap a pipeline in a shared, multicast handle.
    ///
    /// Prefer the [`PipelineShareExt::share`] extension method — it reads as
    /// `pipeline.share()` at the call site.
    pub fn new<P: Pipeline<T>>(p: P) -> Self {
        let upstream: Arc<dyn UpstreamHandle<T>> = Arc::new(UpstreamWrap(p));
        Self {
            inner: Arc::new(SharedPipelineInner {
                upstream,
                upstream_guard: Mutex::new(None),
                subscribers: ArcSwap::from_pointee(Vec::new()),
                subs_writer: Mutex::new(()),
            }),
        }
    }
}

impl<T: CellValue> Gettable<T> for SharedPipeline<T> {
    fn get(&self) -> T {
        // Recompute through the upstream. Mirrors how `MapPipeline::get`
        // works on a non-shared chain.
        self.inner.upstream.get()
    }
}

impl<T: CellValue> PipelineInstall<T> for SharedPipeline<T> {
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
        let id = Uuid::new_v4();

        // 1. Register the callback. Order matters: if we registered AFTER
        //    installing upstream and upstream emits synchronously inside the
        //    install call, this subscriber would miss that initial signal.
        self.inner.add_subscriber(id, callback);

        // 2. Ensure the upstream is subscribed exactly once. The first install
        //    after construction (or after a drain-and-revive race) wins; the
        //    others see `Some(_)` and bail out.
        {
            let mut guard_slot = self
                .inner
                .upstream_guard
                .lock()
                .expect("share upstream_guard poisoned");
            if guard_slot.is_none() {
                let weak = Arc::downgrade(&self.inner);
                let fanout: Arc<dyn Fn(&Signal<T>) + Send + Sync> =
                    Arc::new(move |signal: &Signal<T>| {
                        let Some(inner) = weak.upgrade() else {
                            return;
                        };
                        // Hot path: lock-free Arc bump on the subscriber list,
                        // then iterate and invoke each callback. No mutex.
                        let subs = inner.subscribers.load();
                        for (_, cb) in subs.iter() {
                            cb(signal);
                        }
                    });
                let upstream_guard = self.inner.upstream.install_upstream(fanout);
                *guard_slot = Some(upstream_guard);
            }
        }

        // 3. Build a SubscriptionGuard whose Drop removes this subscriber and
        //    releases the upstream guard if it was the last one.
        let weak = Arc::downgrade(&self.inner);
        SubscriptionGuard::from_callback(move || {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let remaining = inner.remove_subscriber(id);
            if remaining == 0 {
                // Last consumer left — release upstream subscription. A
                // future install on this same SharedPipeline will re-subscribe
                // upstream from scratch.
                let mut slot = inner
                    .upstream_guard
                    .lock()
                    .expect("share upstream_guard poisoned");
                let _drop_outside_lock = slot.take();
                drop(slot);
                // Drop happens here, after releasing the lock, to avoid running
                // upstream cleanup callbacks under our lock.
                drop(_drop_outside_lock);
            }
        })
    }
}

#[allow(private_bounds)]
impl<T: CellValue> Pipeline<T> for SharedPipeline<T> {
    // Default `materialize` is correct: it allocates a Cell that subscribes
    // through `install()` above, which adds one entry to the share-point
    // subscriber list and (on first install) one upstream subscription.
}

/// Extension trait that adds [`share`](PipelineShareExt::share) to any
/// [`Pipeline`].
///
/// `share()` consumes the pipeline and returns a Clone-able
/// [`SharedPipeline`] handle. Each clone of the handle, when materialized
/// (or otherwise installed), adds one fan-out subscriber but does NOT add
/// another upstream subscription — the share point subscribes upstream
/// exactly once, the first time it has a consumer.
pub trait PipelineShareExt<T: CellValue>: Pipeline<T> {
    /// Convert this pipeline into a Clone-able multicast handle.
    fn share(self) -> SharedPipeline<T> {
        SharedPipeline::new(self)
    }
}

impl<T: CellValue, P: Pipeline<T>> PipelineShareExt<T> for P {}
