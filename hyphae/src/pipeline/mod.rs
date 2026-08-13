//! Uncompiled reactive operation chains.
//!
//! A [`Pipeline`] is a recipe for a reactive computation — a chain of
//! operators that has not yet been materialized into a [`Cell`]. Both pure
//! transforms (`map`, `filter`, ...) and operators with install-local state
//! (`debounce`, `buffer_count`, `join`, ...) remain lazy until this boundary.
//! Pipelines deliberately do not implement `subscribe` or expose a public
//! `get`: to observe output you must call `.materialize()`, which installs the
//! required root subscriptions and returns a subscribable cell.
//!
//! # Seedness
//!
//! Pipelines carry a [`Seedness`] type marker indicating whether they have a
//! definite initial value at the moment of materialization:
//!
//! - [`Definite`] — every emission is a real value of `T`. `.materialize()`
//!   returns `Cell<T, CellImmutable>` via [`Materialize`]. Used by `map`,
//!   `tap`, `try_map`, `map_ok`, etc.
//! - [`Empty`] — the pipeline may swallow the synchronous-on-subscribe initial
//!   emission (e.g. `filter` whose predicate fails on the source's initial
//!   value). `.materialize()` returns `Cell<Option<T>, CellImmutable>` via
//!   [`Materialize`], initialized to `None`. The cell transitions monotonically
//!   `None → Some(T)` once the first emission lands; subsequent failures do not
//!   revert.
//!
//! Operators that may swallow the initial value force `S = Empty`, and
//! downstream operators (`map`, `tap`, ...) propagate `S` through the chain.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::CellValue,
};

pub(crate) mod cell_impl;
pub mod share;

pub use share::{PipelineShareExt, SharedPipeline};

mod sealed {
    pub trait Sealed {}
}

/// Type-level marker on [`Pipeline`] indicating whether the pipeline is
/// guaranteed to have a definite initial value at materialize time.
///
/// See module docs for the [`Definite`] / [`Empty`] distinction.
#[allow(private_bounds)]
pub trait Seedness: sealed::Sealed + Send + Sync + 'static {
    #[doc(hidden)]
    type Materialized<T: CellValue>: CellValue;
    #[doc(hidden)]
    type Seed<T: CellValue>: Send + Sync + 'static;
}

/// Pipeline has a guaranteed initial value (`materialize → Cell<T>`).
pub struct Definite;

/// Pipeline may have no initial value (`materialize → Cell<Option<T>>`).
pub struct Empty;

impl sealed::Sealed for Definite {}
impl sealed::Sealed for Empty {}
#[allow(private_bounds)]
impl Seedness for Definite {
    type Materialized<T: CellValue> = T;
    type Seed<T: CellValue> = T;
}
#[allow(private_bounds)]
impl Seedness for Empty {
    type Materialized<T: CellValue> = Option<T>;
    type Seed<T: CellValue> = ();
}

/// Crate-private installer hook used by `materialize`.
///
/// `install` subscribes the pipeline's composed callback to the root source
/// and returns the guard. The fused closure transforms root-source signals
/// into the pipeline's output signal type and invokes the provided callback.
pub(crate) trait PipelineInstall<T: CellValue>: Send + Sync + 'static {
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard;
}

pub(crate) trait PipelineMaterialize<T: CellValue, S: Seedness>: PipelineInstall<T> {
    fn pipeline_seed(&self) -> S::Seed<T>;

    fn materialize_pipeline(self) -> Cell<S::Materialized<T>, CellImmutable>
    where
        Self: Sized;
}

impl<P, T> PipelineMaterialize<T, Definite> for P
where
    P: PipelineInstall<T> + PipelineSeed<T>,
    T: CellValue,
{
    fn pipeline_seed(&self) -> T {
        self.seed()
    }

    fn materialize_pipeline(self) -> Cell<T, CellImmutable> {
        self.materialize_definite()
    }
}

impl<P, T> PipelineMaterialize<T, Empty> for P
where
    P: PipelineInstall<T>,
    T: CellValue,
{
    fn pipeline_seed(&self) {}

    fn materialize_pipeline(self) -> Cell<Option<T>, CellImmutable> {
        let cell = Cell::<Option<T>, CellMutable>::new(None);
        let weak = cell.downgrade();
        let callback: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal| {
            if let Some(cell) = weak.upgrade() {
                cell.notify(signal.map(|value| Some(value.clone())));
            }
        });
        let guard = self.install(callback);
        cell.own(guard);
        cell.lock()
    }
}

enum SignalQueue<T> {
    Empty,
    One(Signal<T>),
    Many(Vec<Signal<T>>),
}

impl<T: CellValue> SignalQueue<T> {
    fn push(&mut self, signal: Signal<T>) {
        match self {
            Self::Empty => *self = Self::One(signal),
            Self::One(_) => {
                let previous = std::mem::replace(self, Self::Empty);
                *self = match previous {
                    Self::One(first) => Self::Many(vec![first, signal]),
                    Self::Empty => Self::One(signal),
                    Self::Many(mut signals) => {
                        signals.push(signal);
                        Self::Many(signals)
                    }
                };
            }
            Self::Many(signals) => signals.push(signal),
        }
    }

    fn latest_value(&self) -> Option<(T, usize)> {
        match self {
            Self::One(Signal::Value(value)) => Some((value.as_ref().clone(), 1)),
            Self::Empty | Self::One(_) => None,
            Self::Many(signals) => signals
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, signal)| match signal {
                    Signal::Value(value) => Some((value.as_ref().clone(), index.saturating_add(1))),
                    _ => None,
                }),
        }
    }

    fn discard_prefix(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        match self {
            Self::Empty => {}
            Self::One(_) => *self = Self::Empty,
            Self::Many(signals) => {
                let mut tail = signals.split_off(count.min(signals.len()));
                *self = if tail.is_empty() {
                    Self::Empty
                } else if tail.len() == 1 {
                    Self::One(tail.remove(0))
                } else {
                    Self::Many(tail)
                };
            }
        }
    }

    const fn take(&mut self) -> Self {
        std::mem::replace(self, Self::Empty)
    }

    const fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    fn deliver(self, callback: &Arc<dyn Fn(&Signal<T>) + Send + Sync>) {
        match self {
            Self::Empty => {}
            Self::One(signal) => callback(&signal),
            Self::Many(signals) => {
                for signal in signals {
                    callback(&signal);
                }
            }
        }
    }
}

enum InstallCapture<T> {
    Capturing(SignalQueue<T>),
    Activating(SignalQueue<T>),
    Live(Arc<dyn Fn(&Signal<T>) + Send + Sync>),
}

/// A definite pipeline subscription installed before its observation cell exists.
///
/// Signals emitted synchronously by `install` are retained until the caller has
/// created the cell from `initial`. Activation then publishes any signals that
/// arrived after the chosen initial value before switching atomically to live
/// forwarding. This closes the former `seed(); install()` lost-update window.
pub(crate) struct PreparedInstall<T: CellValue> {
    initial: T,
    state: Arc<Mutex<InstallCapture<T>>>,
    guard: SubscriptionGuard,
}

impl<T: CellValue> PreparedInstall<T> {
    pub(crate) const fn initial(&self) -> &T {
        &self.initial
    }

    pub(crate) fn activate(
        self,
        callback: &Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
        let mut pending = {
            let mut state = self.state.lock();
            let pending = match &mut *state {
                InstallCapture::Capturing(signals) | InstallCapture::Activating(signals) => {
                    signals.take()
                }
                InstallCapture::Live(_) => SignalQueue::Empty,
            };
            *state = InstallCapture::Activating(SignalQueue::Empty);
            pending
        };

        loop {
            pending.deliver(callback);
            pending = {
                let mut state = self.state.lock();
                match &mut *state {
                    InstallCapture::Activating(queued) if queued.is_empty() => {
                        *state = InstallCapture::Live(Arc::clone(callback));
                        drop(state);
                        break;
                    }
                    InstallCapture::Activating(queued) | InstallCapture::Capturing(queued) => {
                        queued.take()
                    }
                    InstallCapture::Live(_) => break,
                }
            };
        }
        self.guard
    }
}

/// Seed hook used to initialize the materialized cell for [`Definite`]
/// pipelines.
///
/// Only [`Definite`] pipelines implement this trait — [`Empty`] pipelines have
/// no honest initial value and instead seed `None` at the cell boundary.
///
/// This trait is sealed via the crate-private [`PipelineInstall`] supertrait,
/// so external crates can name it (so it can appear in pipeline operator
/// return types) but cannot implement it. It is intentionally not part of the
/// `Pipeline<T, Definite>` supertrait list — the seed mechanism is an
/// implementation detail of materialization, not a public way to read pipeline
/// values pre-materialization.
///
/// `seed()` is allowed to recompute through the source on every call. It is
/// only ever invoked once per `materialize` call.
#[allow(private_bounds)]
pub trait PipelineSeed<T: CellValue>: PipelineInstall<T> {
    #[doc(hidden)]
    fn seed(&self) -> T;

    #[doc(hidden)]
    fn materialize_definite(self) -> Cell<T, CellImmutable>
    where
        Self: Sized,
    {
        let prepared = prepare_install(&self);
        let cell = Cell::<T, CellMutable>::new(prepared.initial().clone());
        let weak = cell.downgrade();
        let callback: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal| {
            if let Some(cell) = weak.upgrade() {
                cell.notify(signal.clone());
            }
        });
        let guard = prepared.activate(&callback);
        cell.own(guard);
        cell.lock()
    }
}

/// Subscribe before choosing the materialized initial value.
///
/// Most definite pipelines synchronously replay a value from `install`, so
/// that replay is both fresher and cheaper than separately walking the plan
/// through `seed()`. Operators that intentionally delay their first replay
/// fall back to `seed()` while the installed callback continues capturing
/// concurrent signals.
pub(crate) fn prepare_install<P, T>(pipeline: &P) -> PreparedInstall<T>
where
    P: PipelineSeed<T>,
    T: CellValue,
{
    let state = Arc::new(Mutex::new(InstallCapture::Capturing(SignalQueue::Empty)));
    let capture = state.clone();
    let guard = pipeline.install(Arc::new(move |signal| {
        let live = {
            let mut state = capture.lock();
            match &mut *state {
                InstallCapture::Capturing(signals) => {
                    signals.push(signal.clone());
                    None
                }
                InstallCapture::Activating(queued) => {
                    queued.push(signal.clone());
                    None
                }
                InstallCapture::Live(callback) => Some(callback.clone()),
            }
        };
        if let Some(callback) = live {
            callback(signal);
        }
    }));

    let captured_initial = {
        let state = state.lock();
        match &*state {
            InstallCapture::Capturing(signals) | InstallCapture::Activating(signals) => {
                signals.latest_value()
            }
            InstallCapture::Live(_) => None,
        }
    };

    let (initial, initial_boundary) = captured_initial.unwrap_or_else(|| {
        let fallback = pipeline.seed();
        let state = state.lock();
        match &*state {
            InstallCapture::Capturing(signals) | InstallCapture::Activating(signals) => {
                signals.latest_value().unwrap_or((fallback, 0))
            }
            InstallCapture::Live(_) => (fallback, 0),
        }
    });

    {
        let mut state = state.lock();
        if let InstallCapture::Capturing(signals) | InstallCapture::Activating(signals) =
            &mut *state
        {
            signals.discard_prefix(initial_boundary);
        }
    }

    PreparedInstall {
        initial,
        state,
        guard,
    }
}

/// Uncompiled reactive operation chain.
///
/// Pipelines are built by chaining operators on a source (`Cell` or another
/// `Pipeline`). They deliberately do not expose `subscribe` or a public `get`
/// — call `.materialize()` to produce a subscribable [`Cell`].
///
/// The `S: Seedness` parameter tracks whether the pipeline has a definite
/// initial value. See module docs.
///
/// # Sealing
///
/// The `PipelineInstall<T>` supertrait is `pub(crate)`, which seals
/// `Pipeline` so external crates cannot define new `Pipeline` types.
///
/// # Not `Clone`
///
/// Pipelines are deliberately not `Clone`. Cloning would duplicate the
/// composed closure work. To share work across consumers, materialize once
/// into a [`Cell`] (clone is an `Arc` bump on the multicast cache) or use
/// [`PipelineShareExt::share`].
#[allow(private_bounds)]
pub trait Pipeline<T: CellValue, S: Seedness = Definite>:
    PipelineInstall<T> + PipelineMaterialize<T, S> + Sized + Send + Sync + 'static
{
}

/// Compile a pipeline into the cell shape selected by its [`Seedness`].
#[allow(private_bounds)]
pub trait Materialize<T: CellValue, S: Seedness>: Pipeline<T, S> {
    fn materialize(self) -> Cell<S::Materialized<T>, CellImmutable>;
}

#[allow(private_bounds)]
impl<P, T, S> Materialize<T, S> for P
where
    P: Pipeline<T, S>,
    T: CellValue,
    S: Seedness,
{
    #[track_caller]
    fn materialize(self) -> Cell<S::Materialized<T>, CellImmutable> {
        self.materialize_pipeline()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Watchable};

    struct StaleSeed(Cell<u64, CellMutable>);

    impl PipelineInstall<u64> for StaleSeed {
        fn install(&self, callback: Arc<dyn Fn(&Signal<u64>) + Send + Sync>) -> SubscriptionGuard {
            self.0.subscribe(move |signal| callback(signal))
        }
    }

    impl PipelineSeed<u64> for StaleSeed {
        fn seed(&self) -> u64 {
            0
        }
    }

    impl Pipeline<u64, Definite> for StaleSeed {}

    #[test]
    fn materialize_prefers_installed_replay_over_a_stale_seed() {
        let source = Cell::new(7_u64);
        let materialized = Materialize::materialize(StaleSeed(source));
        assert_eq!(materialized.get(), 7);
    }

    struct SynchronousBurst;

    impl PipelineInstall<u64> for SynchronousBurst {
        fn install(&self, callback: Arc<dyn Fn(&Signal<u64>) + Send + Sync>) -> SubscriptionGuard {
            callback(&Signal::value(1));
            callback(&Signal::value(2));
            callback(&Signal::Complete);
            SubscriptionGuard::combine(Vec::new())
        }
    }

    impl PipelineSeed<u64> for SynchronousBurst {
        fn seed(&self) -> u64 {
            0
        }
    }

    impl Pipeline<u64, Definite> for SynchronousBurst {}

    #[test]
    fn materialize_uses_freshest_sync_value_and_replays_following_terminal() {
        let materialized = Materialize::materialize(SynchronousBurst);
        assert_eq!(materialized.get(), 2);
        assert!(materialized.is_complete());
    }
}
