use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::cell_map::MapDiff;

use super::BoxedMapDiffSink;

type QueuedQueryEvent = Box<dyn FnOnce() + Send + 'static>;

pub const QUERY_POISONED_MESSAGE: &str =
    "hyphae join region is poisoned after a prior callback panic";

/// Fail-stop cohort shared by every physical root compiled into one query.
#[derive(Clone, Default)]
pub struct QueryPoison(Arc<AtomicBool>);

impl QueryPoison {
    pub fn poison(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_poisoned(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// One publication transaction gate shared by every physical root in a query.
///
/// The first event remains borrowed and statically typed. Only an event that
/// arrives behind active fanout is cloned and type-erased into the FIFO.
#[derive(Default)]
pub(super) struct QueryDispatch {
    pub(super) active: bool,
    pub(super) queued: VecDeque<QueuedQueryEvent>,
}

struct ActiveQueryDispatch<'a> {
    dispatch: &'a Mutex<QueryDispatch>,
    armed: bool,
}

impl Drop for ActiveQueryDispatch<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut state = self
                .dispatch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active = false;
            // A panicking transaction may have reached only some consumers.
            // Events queued behind that partial commit cannot safely run.
            state.queued.clear();
        }
    }
}

#[cold]
#[allow(clippy::panic)]
fn panic_query_poisoned() -> ! {
    std::panic::panic_any(QUERY_POISONED_MESSAGE);
}

fn fanout_root_diff<K, V>(sinks: &[BoxedMapDiffSink<K, V>], diff: &MapDiff<K, V>) {
    for sink in sinks {
        sink(diff);
    }
}

pub(super) fn dispatch_query_root<K, V>(
    dispatch: &Mutex<QueryDispatch>,
    poison: &QueryPoison,
    sinks: &Arc<Vec<BoxedMapDiffSink<K, V>>>,
    diff: &MapDiff<K, V>,
) where
    K: Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    {
        let mut state = dispatch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if poison.is_poisoned() {
            drop(state);
            panic_query_poisoned();
        }
        if state.active {
            let sinks = Arc::clone(sinks);
            let diff = diff.clone();
            state
                .queued
                .push_back(Box::new(move || fanout_root_diff(&sinks, &diff)));
            return;
        }
        state.active = true;
    }

    let mut active = ActiveQueryDispatch {
        dispatch,
        armed: true,
    };
    fanout_root_diff(sinks, diff);

    loop {
        let mut state = dispatch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(next) = state.queued.pop_front() else {
            state.active = false;
            active.armed = false;
            drop(state);
            return;
        };
        drop(state);
        next();
    }
}
