//! Uncompiled reactive operation chains.
//!
//! A [`Pipeline`] is a recipe for a reactive computation — a chain of pure
//! operators (`map`, `filter`, ...) that has not yet been materialized into
//! a [`Cell`]. Pipelines deliberately do not implement `subscribe` or expose
//! a public `get`: to observe output you must call `.materialize()`, which
//! installs a single subscription on the root source and returns a
//! subscribable cell.
//!
//! # Seedness
//!
//! Pipelines carry a [`Seedness`] type marker indicating whether they have a
//! definite initial value at the moment of materialization:
//!
//! - [`Definite`] — every emission is a real value of `T`. `.materialize()`
//!   returns `Cell<T, CellImmutable>` via [`MaterializeDefinite`]. Used by
//!   `map`, `tap`, `try_map`, `map_ok`, etc.
//! - [`Empty`] — the pipeline may swallow the synchronous-on-subscribe initial
//!   emission (e.g. `filter` whose predicate fails on the source's initial
//!   value). `.materialize()` returns `Cell<Option<T>, CellImmutable>` via
//!   [`MaterializeEmpty`], initialized to `None`. The cell transitions
//!   monotonically `None → Some(T)` once the first emission lands; subsequent
//!   failures do not revert.
//!
//! Operators that may swallow the initial value force `S = Empty`, and
//! downstream operators (`map`, `tap`, ...) propagate `S` through the chain.

use std::sync::Arc;

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
pub trait Seedness: sealed::Sealed + Send + Sync + 'static {}

/// Pipeline has a guaranteed initial value (`materialize → Cell<T>`).
pub struct Definite;

/// Pipeline may have no initial value (`materialize → Cell<Option<T>>`).
pub struct Empty;

impl sealed::Sealed for Definite {}
impl sealed::Sealed for Empty {}
impl Seedness for Definite {}
impl Seedness for Empty {}

/// Crate-private installer hook used by `materialize`.
///
/// `install` subscribes the pipeline's composed callback to the root source
/// and returns the guard. The fused closure transforms root-source signals
/// into the pipeline's output signal type and invokes the provided callback.
pub(crate) trait PipelineInstall<T: CellValue>: Send + Sync + 'static {
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard;
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
}

/// Uncompiled reactive operation chain.
///
/// Pipelines are built by chaining pure operators on a source (`Cell` or
/// another `Pipeline`). They deliberately do not expose `subscribe` or a
/// public `get` — call `.materialize()` to produce a subscribable [`Cell`].
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
    PipelineInstall<T> + Sized + Send + Sync + 'static
{
}

/// Compile a [`Definite`] pipeline into a `Cell<T, CellImmutable>`.
///
/// Installs a single subscription on the root source running the fully fused
/// closure and seeds the cell with `self.seed()`. Implementors get the default
/// body for free; the only override in the codebase is on [`Cell`] itself,
/// where materialize is a marker flip on the same `Arc<inner>`.
#[allow(private_bounds)]
pub trait MaterializeDefinite<T: CellValue>:
    Pipeline<T, Definite> + PipelineSeed<T>
{
    #[track_caller]
    fn materialize(self) -> Cell<T, CellImmutable> {
        let initial = self.seed();
        let cell = Cell::<T, CellMutable>::new(initial);
        let weak = cell.downgrade();

        let callback: Arc<dyn Fn(&Signal<T>) + Send + Sync> =
            Arc::new(move |signal: &Signal<T>| {
                if let Some(c) = weak.upgrade() {
                    c.notify(signal.clone());
                }
            });

        let guard = self.install(callback);
        cell.own(guard);
        cell.lock()
    }
}

/// Compile an [`Empty`] pipeline into a `Cell<Option<T>, CellImmutable>`.
///
/// The cell starts as `None`. The install callback lifts each `Signal<T>`
/// into `Signal<Option<T>>` via `Some(value)` before notifying. Once a value
/// has landed, the cell stays `Some(_)` — failing emissions never reach the
/// cell and so cannot revert it.
#[allow(private_bounds)]
pub trait MaterializeEmpty<T: CellValue>: Pipeline<T, Empty> {
    #[track_caller]
    fn materialize(self) -> Cell<Option<T>, CellImmutable> {
        let cell = Cell::<Option<T>, CellMutable>::new(None);
        let weak = cell.downgrade();

        let callback: Arc<dyn Fn(&Signal<T>) + Send + Sync> =
            Arc::new(move |signal: &Signal<T>| {
                if let Some(c) = weak.upgrade() {
                    let lifted: Signal<Option<T>> = signal.map(|v| Some(v.clone()));
                    c.notify(lifted);
                }
            });

        let guard = self.install(callback);
        cell.own(guard);
        cell.lock()
    }
}
