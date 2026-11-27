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

use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use uuid::Uuid;

use crate::{
    cell::{Cell, CellImmutable, CellMutable, WeakCell},
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

/// A signal passed between fused stages.
///
/// A stage that computes a fresh value hands it on by value, so the value is
/// only put behind an `Arc` where something keeps it (the output cell, or an
/// operator that retains it). A value that already arrived shared is passed
/// on as the same `Arc`.
pub(crate) enum Emit<'a, T> {
    Shared(&'a Arc<T>),
    Owned(T),
    Complete,
    Error(&'a Arc<anyhow::Error>),
}

pub(crate) type EmitSink<T> = Arc<dyn for<'a> Fn(Emit<'a, T>) + Send + Sync>;

impl<'a, T> Emit<'a, T> {
    pub(crate) const fn from_signal(signal: &'a Signal<T>) -> Self {
        match signal {
            Signal::Value(value) => Self::Shared(value),
            Signal::Complete => Self::Complete,
            Signal::Error(error) => Self::Error(error),
        }
    }

    pub(crate) fn into_signal(self) -> Signal<T> {
        match self {
            Self::Shared(value) => Signal::Value(Arc::clone(value)),
            Self::Owned(value) => Signal::value(value),
            Self::Complete => Signal::Complete,
            Self::Error(error) => Signal::Error(Arc::clone(error)),
        }
    }
}

/// Crate-private installer hook used by `materialize`.
///
/// `install` subscribes the pipeline's composed callback to the root source
/// and returns the guard. The fused closure transforms root-source signals
/// into the pipeline's output signal type and invokes the provided callback.
pub(crate) trait PipelineInstall<T: CellValue>: Send + Sync + 'static {
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard;

    /// Install a stage-to-stage sink. Stages that only transform or inspect a
    /// value override this so the value crosses them without an `Arc`.
    fn install_emit(&self, sink: EmitSink<T>) -> SubscriptionGuard {
        self.install(Arc::new(move |signal| sink(Emit::from_signal(signal))))
    }

    /// Install with the materializing cell as the owner of every guard.
    ///
    /// An operator that would otherwise allocate its own cell only to forward
    /// it (`switch_map`, `join`, ...) overrides this to hand its guards to
    /// `owner` and emit straight into `callback`, so the cell `materialize`
    /// creates is the operator's output and each value costs one fanout.
    /// Trailing stages (`map`, `deduped`) deliberately keep this default: run
    /// ahead of the output cell they would execute once per upstream notify
    /// instead of once per settled value under `batch`, and a dedupe would see
    /// coalesced-away intermediates.
    fn install_into(
        &self,
        owner: &Arc<GuardOwner>,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) {
        owner.own(self.install(callback));
    }
}

/// Holds an installing pipeline's guards until its output cell exists, then
/// hands them, and every later one, to that cell.
///
/// Guards must outlive the install call (a `switch_map` re-knit owns a new
/// inner guard on every outer value), but the output cell can only be created
/// once the install has produced its initial value.
pub(crate) struct GuardOwner {
    state: Mutex<OwnerState>,
}

enum OwnerState {
    Pending(Vec<(Option<Uuid>, SubscriptionGuard)>),
    Attached(Arc<dyn OwnerCell>),
}

trait OwnerCell: Send + Sync {
    fn own(&self, key: Option<Uuid>, guard: SubscriptionGuard) -> bool;
    fn is_alive(&self) -> bool;
}

impl<T: CellValue> OwnerCell for WeakCell<T, CellMutable> {
    fn own(&self, key: Option<Uuid>, guard: SubscriptionGuard) -> bool {
        let Some(cell) = self.upgrade() else {
            return false;
        };
        match key {
            Some(key) => cell.own_keyed(key, guard),
            None => cell.own(guard),
        }
        true
    }

    fn is_alive(&self) -> bool {
        Self::is_alive(self)
    }
}

impl GuardOwner {
    fn pending() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(OwnerState::Pending(Vec::new())),
        })
    }

    pub(crate) fn own(&self, guard: SubscriptionGuard) {
        self.place(None, guard);
    }

    /// Own `guard` under `key`, replacing (and dropping) the guard held there.
    /// Returns `false` once the output cell is gone; `guard` is dropped then.
    pub(crate) fn own_keyed(&self, key: Uuid, guard: SubscriptionGuard) -> bool {
        self.place(Some(key), guard)
    }

    /// `false` once the output cell has been dropped.
    pub(crate) fn is_alive(&self) -> bool {
        let cell = match &*self.state.lock() {
            OwnerState::Pending(_) => return true,
            OwnerState::Attached(cell) => Arc::clone(cell),
        };
        cell.is_alive()
    }

    fn place(&self, key: Option<Uuid>, guard: SubscriptionGuard) -> bool {
        let mut state = self.state.lock();
        match &mut *state {
            OwnerState::Pending(guards) => {
                let replaced = key.and_then(|key| {
                    guards
                        .iter()
                        .position(|(held, _)| *held == Some(key))
                        .map(|index| guards.swap_remove(index))
                });
                guards.push((key, guard));
                drop(state);
                drop(replaced);
                true
            }
            OwnerState::Attached(cell) => {
                let cell = Arc::clone(cell);
                drop(state);
                cell.own(key, guard)
            }
        }
    }

    /// Move the pending guards onto `cell`. The lock is held while they move
    /// so a concurrent keyed replacement cannot be overtaken by a stale one.
    fn attach<T: CellValue>(&self, cell: &Cell<T, CellMutable>) {
        let owner: Arc<dyn OwnerCell> = Arc::new(cell.downgrade());
        let mut state = self.state.lock();
        if let OwnerState::Pending(guards) =
            std::mem::replace(&mut *state, OwnerState::Attached(Arc::clone(&owner)))
        {
            for (key, guard) in guards {
                owner.own(key, guard);
            }
        }
    }
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
        let guard = self.install_emit(Arc::new(move |emit| {
            if let Some(cell) = weak.upgrade() {
                cell.notify(match emit {
                    Emit::Shared(value) => Signal::value(Some(T::clone(value))),
                    Emit::Owned(value) => Signal::value(Some(value)),
                    Emit::Complete => Signal::Complete,
                    Emit::Error(error) => Signal::Error(Arc::clone(error)),
                });
            }
        }));
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

    /// Remove the latest value and every signal before it, returning that
    /// value. Signals after it stay queued for activation.
    fn take_latest_value(&mut self) -> Option<T> {
        let value = match self.take() {
            Self::One(Signal::Value(value)) => value,
            Self::Empty => return None,
            one @ Self::One(_) => {
                *self = one;
                return None;
            }
            Self::Many(mut signals) => {
                let Some(index) = signals
                    .iter()
                    .rposition(|signal| matches!(signal, Signal::Value(_)))
                else {
                    *self = Self::Many(signals);
                    return None;
                };
                let mut tail = signals.split_off(index.saturating_add(1));
                *self = match tail.len() {
                    0 => Self::Empty,
                    1 => tail.pop().map_or(Self::Empty, Self::One),
                    _ => Self::Many(tail),
                };
                match signals.pop() {
                    Some(Signal::Value(value)) => value,
                    _ => return None,
                }
            }
        };
        // The emitter has returned, so the queue usually holds the only
        // reference and the value moves out without a clone.
        Some(Arc::try_unwrap(value).unwrap_or_else(|value| T::clone(&value)))
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
    Live,
}

struct InstallState<T> {
    capture: Mutex<InstallCapture<T>>,
    live: OnceLock<Arc<dyn Fn(&Signal<T>) + Send + Sync>>,
}

/// A definite pipeline subscription installed before its observation cell exists.
///
/// Signals emitted synchronously by `install` are retained until the caller has
/// created the cell from `initial`. Activation then publishes any signals that
/// arrived after the chosen initial value before switching atomically to live
/// forwarding. This closes the former `seed(); install()` lost-update window.
pub(crate) struct PreparedInstall<T: CellValue, G = SubscriptionGuard> {
    initial: T,
    state: Arc<InstallState<T>>,
    guard: G,
}

impl<T: CellValue, G> PreparedInstall<T, G> {
    pub(crate) const fn initial(&self) -> &T {
        &self.initial
    }

    /// Separate the initial value so a caller can move it into its cell.
    pub(crate) fn into_parts(self) -> (T, PendingActivation<T, G>) {
        (
            self.initial,
            PendingActivation {
                state: self.state,
                guard: self.guard,
            },
        )
    }

    pub(crate) fn activate(self, callback: &Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> G {
        self.into_parts().1.activate(callback)
    }
}

/// A [`PreparedInstall`] whose initial value has been taken.
pub(crate) struct PendingActivation<T: CellValue, G> {
    state: Arc<InstallState<T>>,
    guard: G,
}

impl<T: CellValue, G> PendingActivation<T, G> {
    pub(crate) fn activate(self, callback: &Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> G {
        let mut pending = {
            let mut state = self.state.capture.lock();
            let pending = match &mut *state {
                InstallCapture::Capturing(signals) | InstallCapture::Activating(signals) => {
                    signals.take()
                }
                InstallCapture::Live => SignalQueue::Empty,
            };
            *state = InstallCapture::Activating(SignalQueue::Empty);
            pending
        };

        loop {
            pending.deliver(callback);
            pending = {
                let mut state = self.state.capture.lock();
                match &mut *state {
                    InstallCapture::Activating(queued) if queued.is_empty() => {
                        // Publish only after the activation backlog is empty, while holding
                        // the capture lock so a concurrent slow-path emit cannot be lost.
                        let _ = self.state.live.set(Arc::clone(callback));
                        *state = InstallCapture::Live;
                        drop(state);
                        break;
                    }
                    InstallCapture::Activating(queued) | InstallCapture::Capturing(queued) => {
                        queued.take()
                    }
                    InstallCapture::Live => break,
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
        materialize_mutable(&self).lock()
    }
}

/// Install `pipeline` into a new cell that owns its guards.
///
/// The cell is the pipeline's output: an operator overriding
/// [`PipelineInstall::install_into`] emits into it directly, so no forwarding
/// cell sits between the operator and its subscribers.
pub(crate) fn materialize_mutable<P, T>(pipeline: &P) -> Cell<T, CellMutable>
where
    P: PipelineSeed<T>,
    T: CellValue,
{
    let owner = GuardOwner::pending();
    let prepared = prepare(pipeline, |callback| pipeline.install_into(&owner, callback));
    let (initial, prepared) = prepared.into_parts();
    let cell = Cell::<T, CellMutable>::new(initial);
    owner.attach(&cell);
    let weak = cell.downgrade();
    let callback: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal| {
        if let Some(cell) = weak.upgrade() {
            cell.notify(signal.clone());
        }
    });
    prepared.activate(&callback);
    cell
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
    prepare(pipeline, |callback| pipeline.install(callback))
}

fn prepare<P, T, G>(
    pipeline: &P,
    install: impl FnOnce(Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> G,
) -> PreparedInstall<T, G>
where
    P: PipelineSeed<T>,
    T: CellValue,
{
    let state = Arc::new(InstallState {
        capture: Mutex::new(InstallCapture::Capturing(SignalQueue::Empty)),
        live: OnceLock::new(),
    });
    let capture = state.clone();
    let guard = install(Arc::new(move |signal| {
        if let Some(callback) = capture.live.get() {
            callback(signal);
            return;
        }
        let live = {
            let mut state = capture.capture.lock();
            match &mut *state {
                InstallCapture::Capturing(signals) => {
                    signals.push(signal.clone());
                    None
                }
                InstallCapture::Activating(queued) => {
                    queued.push(signal.clone());
                    None
                }
                InstallCapture::Live => capture.live.get(),
            }
        };
        if let Some(callback) = live {
            callback(signal);
        }
    }));

    let take_captured = || {
        let mut state = state.capture.lock();
        match &mut *state {
            InstallCapture::Capturing(signals) | InstallCapture::Activating(signals) => {
                signals.take_latest_value()
            }
            InstallCapture::Live => None,
        }
    };
    let initial = take_captured().unwrap_or_else(|| {
        let fallback = pipeline.seed();
        take_captured().unwrap_or(fallback)
    });

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

#[cfg(test)]
mod install_tests {
    use super::*;
    use crate::Mutable;

    #[test]
    fn activation_drains_reentrant_signals_before_live_forwarding() {
        let source = Cell::new(0_u32);
        let prepared = prepare_install(&source);
        assert_eq!(*prepared.initial(), 0);
        source.set(1);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let output = seen.clone();
        let input = source.clone();
        let callback: Arc<dyn Fn(&Signal<u32>) + Send + Sync> = Arc::new(move |signal| {
            if let Signal::Value(value) = signal {
                output.lock().push(**value);
                if **value == 1 {
                    input.set(2);
                }
            }
        });
        let _guard = prepared.activate(&callback);
        source.set(3);
        assert_eq!(*seen.lock(), vec![1, 2, 3]);
    }

    #[test]
    fn live_delivery_does_not_acquire_capture_lock() {
        let source = Cell::new(0_u32);
        let prepared = prepare_install(&source);
        let state = prepared.state.clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let output = seen.clone();
        let callback: Arc<dyn Fn(&Signal<u32>) + Send + Sync> = Arc::new(move |signal| {
            if let Signal::Value(value) = signal {
                output.lock().push(**value);
            }
        });
        let _guard = prepared.activate(&callback);
        let capture_lock = state.capture.lock();
        let sender = std::thread::spawn(move || source.set(1));
        let (done, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(sender.join().is_ok());
        });
        let delivered = received.recv_timeout(std::time::Duration::from_secs(5));
        drop(capture_lock);
        assert!(matches!(delivered, Ok(true)));
        assert_eq!(*seen.lock(), vec![1]);
    }
}
