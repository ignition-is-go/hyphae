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
//!
//! # Definite-only
//!
//! `share()` currently requires a [`Definite`] upstream pipeline.
//! [`Empty`](crate::pipeline::Empty) pipelines are not share-able yet — sharing
//! a may-be-empty stream needs additional design for the seed contract.

use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use uuid::Uuid;

use crate::{
    pipeline::{Definite, MaterializeDefinite, Pipeline, PipelineInstall, PipelineSeed},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::CellValue,
};

/// Type-erased upstream handle: holds the original pipeline alive and exposes
/// `seed()` + `install()` so [`SharedPipeline`] can defer materialization.
trait UpstreamHandle<T>: Send + Sync + 'static {
    fn seed(&self) -> T;
    fn install_upstream(&self, sink: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard;
}

struct UpstreamWrap<P>(P);

impl<T, P> UpstreamHandle<T> for UpstreamWrap<P>
where
    T: CellValue,
    P: Pipeline<T, Definite> + PipelineSeed<T>,
{
    fn seed(&self) -> T {
        self.0.seed()
    }
    fn install_upstream(&self, sink: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        self.0.install(sink)
    }
}

type Subscriber<T> = Arc<dyn Fn(&Signal<T>) + Send + Sync>;

pub(crate) struct SharedPipelineInner<T: CellValue> {
    /// Original pipeline kept alive so `seed()` can recompute and so the first
    /// downstream install can subscribe. Type-erased through `UpstreamHandle`.
    upstream: Arc<dyn UpstreamHandle<T>>,
    /// Held subscription on the upstream once the first downstream consumer
    /// installs. `None` until that point and again after the last downstream
    /// subscriber drops.
    upstream_guard: Mutex<Option<SubscriptionGuard>>,
    /// Subscriber registry as `Mutex<Arc<Vec<…>>>`. Reads (fanout) take the
    /// lock briefly to clone the Arc and drop it before invoking callbacks.
    /// Writes (subscribe/unsubscribe) build the next snapshot under the lock
    /// and let the displaced Arc drop *outside* the lock. See
    /// `Cell::subscribers` for the rationale.
    subscribers: parking_lot::Mutex<Arc<Vec<(Uuid, Subscriber<T>)>>>,
}

impl<T: CellValue> SharedPipelineInner<T> {
    fn add_subscriber(&self, id: Uuid, cb: Subscriber<T>) {
        // Swap-and-defer: see Cell::subscribe.
        let _old = {
            let mut guard = self.subscribers.lock();
            let mut next: Vec<(Uuid, Subscriber<T>)> = (**guard).clone();
            next.push((id, cb));
            std::mem::replace(&mut *guard, Arc::new(next))
        };
    }

    /// Returns the remaining subscriber count after removal.
    fn remove_subscriber(&self, id: Uuid) -> usize {
        let (remaining, _old) = {
            let mut guard = self.subscribers.lock();
            let mut next: Vec<(Uuid, Subscriber<T>)> = (**guard)
                .iter()
                .filter(|(i, _)| *i != id)
                .cloned()
                .collect();
            let remaining = next.len();
            next.shrink_to_fit();
            (remaining, std::mem::replace(&mut *guard, Arc::new(next)))
        };
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
/// use hyphae::{Cell, Gettable, MapExt, MaterializeDefinite, Mutable, PipelineShareExt};
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
    /// Prefer the [`PipelineShareExt::share`] extension method.
    #[allow(private_bounds)]
    pub fn new<P: Pipeline<T, Definite> + PipelineSeed<T>>(p: P) -> Self {
        let upstream: Arc<dyn UpstreamHandle<T>> = Arc::new(UpstreamWrap(p));
        Self {
            inner: Arc::new(SharedPipelineInner {
                upstream,
                upstream_guard: Mutex::new(None),
                subscribers: parking_lot::Mutex::new(Arc::new(Vec::new())),
            }),
        }
    }
}

impl<T: CellValue> PipelineSeed<T> for SharedPipeline<T> {
    fn seed(&self) -> T {
        // Recompute through the upstream. Mirrors how `MapPipeline::seed`
        // works on a non-shared chain.
        self.inner.upstream.seed()
    }
}

impl<T: CellValue> PipelineInstall<T> for SharedPipeline<T> {
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
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
                        // Hot path: brief mutex acquire + Arc::clone snapshot,
                        // then iterate callbacks lock-free.
                        let subs = Arc::clone(&*inner.subscribers.lock());
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
                let mut slot = inner
                    .upstream_guard
                    .lock()
                    .expect("share upstream_guard poisoned");
                let _drop_outside_lock = slot.take();
                drop(slot);
                drop(_drop_outside_lock);
            }
        })
    }
}

#[allow(private_bounds)]
impl<T: CellValue> Pipeline<T, Definite> for SharedPipeline<T> {}

impl<T: CellValue> MaterializeDefinite<T> for SharedPipeline<T> {
    // Default body is correct: allocate a Cell, install through
    // PipelineInstall above (which adds one entry to the share-point
    // subscriber list and, on first install, one upstream subscription).
}

/// Extension trait that adds [`share`](PipelineShareExt::share) to any
/// [`Definite`] [`Pipeline`].
///
/// `share()` consumes the pipeline and returns a Clone-able
/// [`SharedPipeline`] handle. Each clone of the handle, when materialized
/// (or otherwise installed), adds one fan-out subscriber but does NOT add
/// another upstream subscription — the share point subscribes upstream
/// exactly once, the first time it has a consumer.
#[allow(private_bounds)]
pub trait PipelineShareExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    fn share(self) -> SharedPipeline<T> {
        SharedPipeline::new(self)
    }
}

impl<T: CellValue, P: Pipeline<T, Definite> + PipelineSeed<T>> PipelineShareExt<T> for P {}
