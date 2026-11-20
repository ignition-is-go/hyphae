//! Who fires ready nodes: pool threads, and writers that wait or help.
//!
//! A node that is ready to fire sits in one queue with its claim held. Any
//! thread may pop it and fire it. A waiting writer helps instead of
//! sleeping. `HYPHAE_SETTLE_WORKERS=0` still runs one pool thread, for the
//! work no writer waits for.
//!
//! The pool is plain threads, not the Rayon pool in `executor.rs`, for the
//! reason `docs/settle-core.md` section 12 gives.

use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

use crossbeam::queue::SegQueue;
use parking_lot::{Condvar, Mutex};

use super::{INBOX, Node, admit_drain, run};

const MAX_WORKERS: usize = 256;

/// `HYPHAE_SETTLE_WORKERS` is the number of pool threads; 0, the default,
/// leaves all firing to the writers.
static WORKERS: LazyLock<AtomicUsize> = LazyLock::new(|| {
    AtomicUsize::new(
        std::env::var("HYPHAE_SETTLE_WORKERS")
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0usize)
            .min(MAX_WORKERS),
    )
});

static READY: SegQueue<Arc<dyn Node>> = SegQueue::new();
static GENERATION: AtomicU64 = AtomicU64::new(0);
static SLEEPERS: AtomicUsize = AtomicUsize::new(0);
static SLEEP: Mutex<()> = Mutex::new(());
static WAKE: Condvar = Condvar::new();
static SPAWNED: Mutex<usize> = Mutex::new(0);

static WAVE_MIN: AtomicUsize = AtomicUsize::new(1);

/// Kept for source compatibility: there are no waves, and the value is
/// unused. Returns the previous value.
#[doc(hidden)]
#[allow(clippy::must_use_candidate, reason = "callers set it for its effect")]
pub fn set_wave_min(nodes: usize) -> usize {
    WAVE_MIN.swap(nodes.max(1), Ordering::AcqRel)
}

/// Set the number of pool threads and return the previous value. For tests.
#[doc(hidden)]
pub fn set_workers(threads: usize) -> usize {
    let previous = WORKERS.swap(threads.min(MAX_WORKERS), Ordering::AcqRel);
    spawn_workers();
    notify();
    previous
}

pub(super) fn workers() -> usize {
    WORKERS.load(Ordering::Acquire)
}

/// Pool threads that run. A write a callback makes is a later ticket that
/// no writer waits for, and `submit` and `enqueue` leave work behind, so at
/// least one thread must run what no writer is helping with. With
/// `HYPHAE_SETTLE_WORKERS=0` that is one thread.
fn running() -> usize {
    workers().max(1)
}

fn spawn_workers() {
    let mut spawned = SPAWNED.lock();
    while *spawned < running() {
        let index = *spawned;
        let started = std::thread::Builder::new()
            .name(format!("hyphae-settle-{index}"))
            .spawn(move || worker(index));
        if let Err(error) = started {
            log::error!("hyphae: cannot start a settle pool thread: {error}");
            return;
        }
        *spawned = spawned.saturating_add(1);
    }
    drop(spawned);
}

fn worker(index: usize) {
    loop {
        if index < running() {
            if !INBOX.is_empty() {
                admit_drain();
            }
            if let Some(node) = READY.pop() {
                run(node, true);
                continue;
            }
        }
        sleep(&|| index < running() && (!READY.is_empty() || !INBOX.is_empty()));
    }
}

/// Queue a claimed node that is ready to fire.
pub(super) fn ready(node: Arc<dyn Node>) {
    READY.push(node);
    notify();
}

/// Wake sleeping threads: work is ready, or a ticket completed.
pub(super) fn notify() {
    GENERATION.fetch_add(1, Ordering::AcqRel);
    if SLEEPERS.load(Ordering::Acquire) > 0 {
        let _sleep = SLEEP.lock();
        WAKE.notify_all();
    }
}

fn sleep(wake: &dyn Fn() -> bool) {
    let mut guard = SLEEP.lock();
    SLEEPERS.fetch_add(1, Ordering::AcqRel);
    let generation = GENERATION.load(Ordering::Acquire);
    if !wake() {
        while GENERATION.load(Ordering::Acquire) == generation {
            WAKE.wait(&mut guard);
        }
    }
    SLEEPERS.fetch_sub(1, Ordering::AcqRel);
}

/// Fire ready nodes on this thread until `done` holds. With `idle_ends`,
/// also stop once nothing is ready or queued, or after a bounded number of
/// fires, so that writers on other threads cannot keep this one here.
pub(super) fn help_until(done: &dyn Fn() -> bool, idle_ends: bool) {
    const IDLE_BUDGET: usize = 4096;
    spawn_workers();
    let mut fired = 0usize;
    loop {
        if idle_ends && fired >= IDLE_BUDGET {
            return;
        }
        fired = fired.saturating_add(1);
        if done() {
            return;
        }
        if !INBOX.is_empty() {
            admit_drain();
        }
        if let Some(node) = READY.pop() {
            run(node, false);
            continue;
        }
        if idle_ends && INBOX.is_empty() {
            return;
        }
        sleep(&|| done() || !READY.is_empty() || !INBOX.is_empty());
    }
}
