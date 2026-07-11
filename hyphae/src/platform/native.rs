//! Native (threaded) implementation of the platform primitives.

pub use std::time::Instant;
use std::{
    sync::{Condvar, Mutex, OnceLock},
    thread,
    time::Duration,
};

use rayon::prelude::*;

/// Below this remaining wait, busy-spin for accuracy instead of parking on
/// the condvar. `Condvar::wait_timeout` (backed by the OS scheduler) has
/// millisecond-ish granularity; `spin_sleep` gets sub-millisecond precision
/// for the final approach to a deadline.
const SPIN_THRESHOLD: Duration = Duration::from_millis(2);

/// A single scheduled timer, owned by the shared [`Reactor`].
struct TimerEntry {
    next_fire: Instant,
    period: Duration,
    count: u64,
    tick: Box<dyn FnMut(u64) -> bool + Send>,
}

/// Process-wide timer reactor: every [`spawn_interval`]/[`spawn_delayed`] call
/// registers a [`TimerEntry`] here instead of spawning its own OS thread.
///
/// This is load-bearing for hyphae's `scheduler` feature, not just a thread-
/// count optimization. The scheduler's glitch-free coalescing/height-ordering
/// (`crate::scheduler`) is a *thread-local* guarantee — it only holds for
/// propagation that happens on one thread. Two timer-driven `Source`s that
/// each spawned their own thread (the old per-call `thread::spawn` design)
/// can fan out into a shared downstream cell concurrently from different
/// threads, and nothing coordinates them: the "exactly once, glitch-free"
/// guarantee each thread believes it owns silently doesn't hold at the
/// convergence point, and depending on scheduling a genuine value change can
/// be overwritten before any subscriber observes it. Running every interval
/// and delayed timer off one reactor thread makes the thread-local guarantee
/// a process-wide one for the entire timer-driven subgraph, closing that
/// hole instead of relying on callers to avoid ever combining two
/// independently-timed sources.
struct Reactor {
    entries: Mutex<Vec<TimerEntry>>,
    wake: Condvar,
}

fn reactor() -> &'static Reactor {
    static REACTOR: OnceLock<&'static Reactor> = OnceLock::new();
    REACTOR.get_or_init(|| {
        let reactor: &'static Reactor = Box::leak(Box::new(Reactor {
            entries: Mutex::new(Vec::new()),
            wake: Condvar::new(),
        }));
        thread::Builder::new()
            .name("hyphae-timer-reactor".into())
            .spawn(move || run_reactor(reactor))
            .expect("failed to spawn hyphae timer reactor thread");
        reactor
    })
}

fn register(entry: TimerEntry) {
    let reactor = reactor();
    reactor.entries.lock().unwrap().push(entry);
    // A freshly-registered entry may have an earlier deadline than whatever
    // the reactor is currently parked/spinning on — wake it so it recomputes.
    reactor.wake.notify_all();
}

fn run_reactor(reactor: &'static Reactor) -> ! {
    loop {
        let due = wait_for_due(reactor);
        for mut entry in due {
            // Isolate a panicking tick callback. Under the old one-thread-per-
            // timer design a panicking subscriber killed only that timer's own
            // thread; on the shared reactor an unguarded panic would unwind
            // `run_reactor`, kill the single reactor thread, and silently stop
            // EVERY timer process-wide (the `OnceLock` never respawns it). Catch
            // it and drop just this entry, matching the old blast radius — the
            // same per-op isolation `scheduler::run_wave` already applies.
            let keep = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (entry.tick)(entry.count)
            }))
            .unwrap_or(false);
            if !keep {
                continue;
            }

            entry.count += 1;
            entry.next_fire += entry.period;
            // Drift catch-up: if the reactor fell behind (e.g. a slow tick
            // callback, or many timers due at once), fold the missed ticks
            // into `count` and resync to one full period out — NOT to `now`,
            // which would make the entry immediately due again and fire a
            // zero-gap makeup tick (and, under sustained overload where a
            // callback is slower than `period`, hammer with no breathing room).
            // `now + period` preserves the documented fixed-rate semantics.
            let now = Instant::now();
            if entry.next_fire < now {
                let missed =
                    ((now - entry.next_fire).as_nanos() / entry.period.as_nanos().max(1)) as u64;
                entry.count += missed;
                entry.next_fire = now + entry.period;
            }

            reactor.entries.lock().unwrap().push(entry);
        }
    }
}

/// Block until at least one entry is due, then remove and return every entry
/// whose deadline has passed. Ticks never run while the lock is held — a
/// tick callback that re-enters `spawn_interval`/`spawn_delayed` (registering
/// a new timer) would otherwise deadlock on this same mutex.
fn wait_for_due(reactor: &'static Reactor) -> Vec<TimerEntry> {
    let mut guard = reactor.entries.lock().unwrap();
    loop {
        if guard.is_empty() {
            guard = reactor.wake.wait(guard).unwrap();
            continue;
        }

        let now = Instant::now();
        // Small N in practice (the process's distinct interval/delayed-timer
        // count, not event volume), so an O(n) scan per wake is cheap and
        // avoids needing a total order (hence a `BinaryHeap`) over entries
        // that carry a non-comparable boxed closure.
        let next_deadline = guard.iter().map(|e| e.next_fire).min().expect("non-empty");

        if next_deadline <= now {
            let now = Instant::now();
            let mut due = Vec::new();
            let mut idx = 0;
            while idx < guard.len() {
                if guard[idx].next_fire <= now {
                    due.push(guard.swap_remove(idx));
                } else {
                    idx += 1;
                }
            }
            return due;
        }

        let wait_for = next_deadline - now;
        if wait_for > SPIN_THRESHOLD {
            // Park for most of the wait; `notify_all` on a new, earlier
            // registration (or the timeout itself) wakes us to re-check.
            let (g, _timed_out) = reactor
                .wake
                .wait_timeout(guard, wait_for - SPIN_THRESHOLD)
                .unwrap();
            guard = g;
            continue;
        }

        // Final approach: busy-spin for sub-millisecond accuracy. Drop the
        // lock first so a concurrent registration isn't blocked on it for
        // the (short) spin.
        drop(guard);
        spin_sleep::sleep(wait_for);
        guard = reactor.entries.lock().unwrap();
    }
}

/// Run `f` once after `delay`, on the shared timer reactor thread.
///
/// The work is fire-and-forget: cancellation is the caller's responsibility
/// (operators use a weak upgrade / generation check inside `f`).
pub fn spawn_delayed(delay: Duration, f: impl FnOnce() + Send + 'static) {
    let mut f = Some(f);
    register(TimerEntry {
        next_fire: Instant::now() + delay,
        period: delay,
        count: 1,
        tick: Box::new(move |_count| {
            if let Some(f) = f.take() {
                f();
            }
            false
        }),
    });
}

/// Repeatedly invoke `tick(count)` every `period`, starting one `period` after
/// the call, with `count` beginning at 1. Stops being rescheduled once `tick`
/// returns `false`.
///
/// `precise` is accepted for API compatibility but no longer changes the
/// wait strategy: the shared reactor always resolves its next wake with a
/// hybrid park-then-spin wait (see [`wait_for_due`]), so every registered
/// timer gets the same sub-millisecond accuracy on the final approach
/// regardless of which other timers are also registered.
pub fn spawn_interval(
    period: Duration,
    _precise: bool,
    tick: impl FnMut(u64) -> bool + Send + 'static,
) {
    register(TimerEntry {
        next_fire: Instant::now() + period,
        period,
        count: 1,
        tick: Box::new(tick),
    });
}

/// Fan out `f` across `items` in parallel using rayon.
pub fn par_for_each<T: Sync>(items: &[T], f: impl Fn(&T) + Send + Sync) {
    items.par_iter().for_each(f);
}
