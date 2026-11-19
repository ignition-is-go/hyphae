//! One ordered entry point for an operator that reads two sources.
//!
//! Two sources of one height fire at the same time. An operator that ran its
//! logic in each source's callback would run it on two threads, in an order
//! that depends on timing. Here both sources push into one cell, and the
//! operator's logic runs when that cell fires: on one thread, in the order a
//! one-thread settle would have delivered the signals.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use super::Watchable;
use crate::{
    cell::{Cell, CellMutable},
    pipeline::PipelineInstall,
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::CellValue,
};

/// A signal from one of the two sources.
#[derive(Clone, Debug)]
pub(super) enum Arrival<L, R> {
    Left(Signal<L>),
    Right(Signal<R>),
}

/// Arrivals are events. No two are the same.
impl<L, R> PartialEq for Arrival<L, R> {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

fn thread_token() -> usize {
    thread_local! {
        static TOKEN: u8 = const { 0 };
    }
    TOKEN.with(|token| std::ptr::from_ref(token).addr())
}

/// Install `left`, then `right`, and run `handle` for each signal of either.
///
/// A signal that a source replays while it is being installed reaches
/// `handle` before `install` returns, on the installing thread. Every later
/// signal reaches it when the funnel cell fires.
pub(super) fn fan_in<L, R>(
    left: &impl PipelineInstall<L>,
    right: &impl PipelineInstall<R>,
    handle: impl Fn(Arrival<L, R>) + Send + Sync + 'static,
) -> SubscriptionGuard
where
    L: CellValue,
    R: CellValue,
{
    let funnel = Cell::<Option<Arrival<L, R>>, CellMutable>::new(None).combine_each();
    let handle = Arc::new(handle);
    let installer = Arc::new(AtomicUsize::new(thread_token()));

    let fired = handle.clone();
    let subscription = funnel.subscribe(move |signal| {
        if let Signal::Value(arrival) = signal
            && let Some(arrival) = arrival.as_ref()
        {
            fired(arrival.clone());
        }
    });

    let weak = funnel.downgrade();
    let replayed = handle.clone();
    let installing = installer.clone();
    let left_guard = left.install(Arc::new(move |signal| {
        let arrival = Arrival::Left(signal.clone());
        if installing.load(Ordering::Acquire) == thread_token() {
            replayed(arrival);
        } else if let Some(funnel) = weak.upgrade() {
            funnel.notify(Signal::value(Some(arrival)));
        }
    }));
    funnel.own(left_guard);

    let weak = funnel.downgrade();
    let installing = installer.clone();
    let right_guard = right.install(Arc::new(move |signal| {
        let arrival = Arrival::Right(signal.clone());
        if installing.load(Ordering::Acquire) == thread_token() {
            handle(arrival);
        } else if let Some(funnel) = weak.upgrade() {
            funnel.notify(Signal::value(Some(arrival)));
        }
    }));
    funnel.own(right_guard);

    installer.store(0, Ordering::Release);
    subscription
}
