//! Uncompiled reactive operation chains.
//!
//! A [`Pipeline`] is a recipe for a reactive computation — a chain of pure
//! operators (`map`, `filter`, ...) that has not yet been materialized into
//! a [`Cell`]. Pipelines deliberately do not implement `subscribe`: to observe
//! output you must call [`Pipeline::materialize`], which installs a single
//! subscription on the root source and returns a subscribable cell.
//!
//! This design makes the memoization boundary explicit. Today, chaining
//! operators on a `Cell` creates an intermediate cell per operator — each
//! caching its value and notifying downstream subscribers. That is the right
//! choice when the derived value is consumed by many subscribers, but wasteful
//! when it is consumed by one. By moving pure operators onto `Pipeline`, the
//! cost of an intermediate cell is paid only when the caller explicitly asks
//! for one with `.materialize()`.

use std::sync::Arc;

use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable},
};

pub(crate) mod cell_impl;

/// Crate-private installer hook used by [`Pipeline::materialize`].
///
/// `install` subscribes the pipeline's composed callback to the root source
/// and returns the guard. The fused closure transforms root-source signals
/// into the pipeline's output signal type and invokes the provided callback.
///
/// This is separate from `Pipeline` so that the public trait stays minimal
/// and cannot be accidentally used to subscribe without materializing.
pub(crate) trait PipelineInstall<T: CellValue>: Send + Sync + 'static {
    /// Install `callback` such that every future value signal from the root
    /// source flows through the pipeline's composed op and invokes `callback`
    /// with the resulting `Signal<T>`.
    ///
    /// The returned guard owns the subscription on the root source. Dropping
    /// it ends the chain. The root-source `DepNode` is accessible via
    /// `guard.source()` for introspection.
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard;
}

/// Uncompiled reactive operation chain.
///
/// Pipelines are built by chaining pure operators on a source (`Cell` or
/// another `Pipeline`). They deliberately do not expose `subscribe` — call
/// [`Pipeline::materialize`] to produce a subscribable [`Cell`].
///
/// # Invariants
///
/// - `get()` recomputes from the root source on every call. Do not use in
///   hot loops — materialize first.
/// - `materialize(self)` consumes the pipeline and installs a single
///   subscription on the root source running the fully fused closure.
/// - No intermediate `Cell` is allocated anywhere in a pipeline chain.
///
/// # Sealing
///
/// The `PipelineInstall<T>` supertrait is `pub(crate)`, which seals
/// `Pipeline` so external crates cannot define new `Pipeline` types. New
/// pipeline shapes are added inside this crate.
///
/// # Not `Clone`
///
/// Pipelines are deliberately not `Clone`. Cloning would duplicate the
/// composed closure work — each clone's `materialize()` installs an
/// independent subscription on the root source, and both run the entire
/// op chain on every emission. To share work across consumers, materialize
/// once into a [`Cell`] (which IS `Clone` — the clone is an `Arc` bump
/// referencing the same multicast cache) and then clone the cell.
#[allow(private_bounds)]
pub trait Pipeline<T: CellValue>:
    Gettable<T> + PipelineInstall<T> + Sized + Send + Sync + 'static
{
    /// Compile the pipeline into a [`Cell`] and install a single subscription
    /// on the root source running the fused closure.
    ///
    /// This is the only way to observe pipeline output. Every subscribe in
    /// the codebase is on a cell, never on a pipeline — which is the point.
    #[track_caller]
    fn materialize(self) -> Cell<T, CellImmutable> {
        let initial = self.get();
        let cell = Cell::<T, CellMutable>::new(initial);
        let weak = cell.downgrade();

        // The pipeline's install() may invoke `callback` synchronously with
        // the current source value on subscription (operators like `map` do).
        // We've already seeded the cell with `pipeline.get()`, so re-notifying
        // with the same value would be redundant. The first-emit skip cannot
        // live inside this callback: operators like `filter` may legitimately
        // swallow the synchronous initial emit (the cold value fails the
        // predicate), and a callback-level guard would then mistakenly skip
        // the *next* legitimate emission. No external subscriber exists during
        // construction, so a redundant initial cell.notify is harmless: it
        // updates the store to the same logical value and notifies zero
        // subscribers. We therefore forward every signal unconditionally.
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
