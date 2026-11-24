//! Shared helpers for the scheduling-domain tests.
#![allow(dead_code)]

use std::{sync::Arc, time::Duration};

use hyphae::Signal;
use parking_lot::{Condvar, Mutex};

pub const DEADLINE: Duration = Duration::from_secs(20);

/// Every test here writes root cells and asserts on them as soon as the write
/// returns. The root tick is shared by the whole process, so a root batch on
/// one test thread can be drained by another test's thread and return before
/// its own propagation is visible. Running the tests one at a time keeps each
/// assertion about root on the thread that drained it.
pub static ROOT_TICK_SERIAL: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct GateState {
    armed: bool,
    entered: bool,
    open: bool,
}

/// Parks whoever calls [`Gate::pass`] once armed, until [`Gate::open`].
#[derive(Default)]
pub struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

impl Gate {
    pub fn arm(&self) {
        self.state.lock().armed = true;
    }

    pub fn is_armed(&self) -> bool {
        self.state.lock().armed
    }

    pub fn pass(&self) {
        let mut state = self.state.lock();
        if !state.armed {
            return;
        }
        state.entered = true;
        self.changed.notify_all();
        while !state.open {
            if self.changed.wait_for(&mut state, DEADLINE).timed_out() {
                return;
            }
        }
    }

    pub fn wait_entered(&self) -> bool {
        let mut state = self.state.lock();
        while !state.entered {
            if self.changed.wait_for(&mut state, DEADLINE).timed_out() {
                return false;
            }
        }
        true
    }

    pub fn is_open(&self) -> bool {
        self.state.lock().open
    }

    pub fn open(&self) {
        self.state.lock().open = true;
        self.changed.notify_all();
    }
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Gate")
    }
}

pub type Log<T> = Arc<Mutex<Vec<T>>>;

pub fn log_values<T: Clone + Send + 'static>(
    log: &Log<T>,
) -> impl Fn(&Signal<T>) + Send + Sync + use<T> {
    let log = Arc::clone(log);
    move |signal| {
        if let Signal::Value(value) = signal {
            log.lock().push(value.as_ref().clone());
        }
    }
}
