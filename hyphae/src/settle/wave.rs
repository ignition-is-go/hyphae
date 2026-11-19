//! Firing one wave of a settle on pool threads.
//!
//! A wave is every frontier entry of one height. No node in it depends on
//! another, so its nodes may fire on other threads than the holder's. The
//! holder still decides the order of everything the wave produces: it merges
//! each stretch of fires by its position in the wave.
//!
//! The pool is a set of plain threads, not the Rayon pool in `executor.rs`. A
//! Rayon thread that blocks inside a callback, as a parallel join region
//! does, runs other queued jobs on the same stack. A second node would then
//! fire inside the first node's callback and under its locks.
//!
//! `docs/settle-core.md` section 12 has the design.

use std::{
    any::Any,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use parking_lot::{Condvar, Mutex};

use super::{
    ACTIVE, Active, Entry, Epoch, Frontier, Outbox, PARALLEL_WAVES, Waiter, fire_entries,
    take_outbox,
};

const DEFAULT_WAVE_MIN: usize = 64;
const MAX_WORKERS: usize = 256;
/// Chunks per thread in a wave. More than one lets a thread that drew cheap
/// nodes take over part of a slow thread's share.
const CHUNKS_PER_WORKER: usize = 8;
const MAX_CHUNK: usize = 256;

struct Switches {
    wave_min: AtomicUsize,
    workers: AtomicUsize,
}

/// `HYPHAE_SETTLE_WORKERS` is the number of threads that fire a wave; 0, the
/// default, fires every wave on the holder. `HYPHAE_SETTLE_WAVE_MIN` is the
/// smallest wave that goes to the pool.
static SWITCHES: LazyLock<Switches> = LazyLock::new(|| Switches {
    wave_min: AtomicUsize::new(
        env_usize("HYPHAE_SETTLE_WAVE_MIN")
            .unwrap_or(DEFAULT_WAVE_MIN)
            .max(1),
    ),
    workers: AtomicUsize::new(
        env_usize("HYPHAE_SETTLE_WORKERS")
            .unwrap_or(0)
            .min(MAX_WORKERS),
    ),
});

fn env_usize(key: &str) -> Option<usize> {
    let value = std::env::var(key).ok()?;
    let parsed = value.trim().parse().ok();
    if parsed.is_none() {
        log::warn!("hyphae: ignoring {key}={value:?}: not a non-negative integer");
    }
    parsed
}

/// Override `HYPHAE_SETTLE_WAVE_MIN` for the whole process. For tests.
#[doc(hidden)]
pub fn set_wave_min(nodes: usize) {
    SWITCHES.wave_min.store(nodes.max(1), Ordering::Relaxed);
}

/// Override `HYPHAE_SETTLE_WORKERS` for the whole process. For tests.
#[doc(hidden)]
pub fn set_workers(threads: usize) {
    SWITCHES
        .workers
        .store(threads.min(MAX_WORKERS), Ordering::Relaxed);
}

/// The number of threads that fire a wave of `len` nodes, or `None` when the
/// holder fires it alone.
pub(super) fn workers_for(len: usize) -> Option<usize> {
    let workers = SWITCHES.workers.load(Ordering::Relaxed);
    if workers == 0 || len < SWITCHES.wave_min.load(Ordering::Relaxed) {
        return None;
    }
    // Several workers include the holder, which fires a lone node itself.
    (workers == 1 || len > 1).then_some(workers)
}

/// One wave while it fires.
struct Wave {
    entries: Vec<Entry>,
    run_id: u64,
    epoch: Option<Epoch>,
    waiters: Arc<[Arc<Waiter>]>,
    /// Entries a thread claims at a time.
    chunk: usize,
    /// Index of the first unclaimed entry.
    next: AtomicUsize,
    /// Pool threads that may still join.
    seats: AtomicUsize,
    /// Entries not yet reported as fired.
    unfired: AtomicUsize,
    stretches: Mutex<Vec<Stretch>>,
    done: Mutex<()>,
    finished: Condvar,
}

/// What one claimed chunk produced.
struct Stretch {
    /// Index in the wave of the chunk's first entry.
    start: usize,
    out: Outbox,
    panic: Option<Box<dyn Any + Send>>,
}

impl Wave {
    /// Claim and fire chunks on this thread until none is left.
    fn work(&self) {
        let mut stretches = Vec::new();
        let mut fired = 0usize;
        loop {
            let start = self.next.fetch_add(self.chunk, Ordering::Relaxed);
            let end = start.saturating_add(self.chunk).min(self.entries.len());
            let Some(entries) = self.entries.get(start..end) else {
                break;
            };
            if entries.is_empty() {
                break;
            }
            let panic = fire_entries(entries);
            stretches.push(Stretch {
                start,
                out: take_outbox(),
                panic,
            });
            fired = fired.saturating_add(entries.len());
        }
        if fired == 0 {
            return;
        }
        self.stretches.lock().append(&mut stretches);
        if self.unfired.fetch_sub(fired, Ordering::AcqRel) == fired {
            let _done = self.done.lock();
            self.finished.notify_one();
        }
    }

    fn take_seat(&self) -> bool {
        self.seats
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |seats| {
                seats.checked_sub(1)
            })
            .is_ok()
    }

    fn wait(&self) {
        let mut done = self.done.lock();
        while self.unfired.load(Ordering::Acquire) != 0 {
            self.finished.wait(&mut done);
        }
    }
}

#[derive(Default)]
struct Posted {
    wave: Option<Arc<Wave>>,
    /// Counts posts, so a thread joins each wave at most once.
    serial: u64,
    threads: usize,
}

static POSTED: Mutex<Posted> = Mutex::new(Posted {
    wave: None,
    serial: 0,
    threads: 0,
});
static WAKE: Condvar = Condvar::new();

/// Offer `wave` to `helpers` pool threads, starting threads as needed. False
/// when the pool has no thread.
fn post(wave: &Arc<Wave>, helpers: usize) -> bool {
    let mut posted = POSTED.lock();
    while posted.threads < helpers {
        let spawned = std::thread::Builder::new()
            .name(format!("hyphae-settle-{}", posted.threads))
            .spawn(pool_thread);
        if let Err(error) = spawned {
            log::warn!("hyphae: could not start a settle pool thread: {error}");
            break;
        }
        posted.threads = posted.threads.saturating_add(1);
    }
    if posted.threads == 0 {
        return false;
    }
    posted.wave = Some(Arc::clone(wave));
    posted.serial = posted.serial.wrapping_add(1);
    drop(posted);
    for _ in 0..helpers {
        WAKE.notify_one();
    }
    true
}

fn pool_thread() {
    let mut joined = 0u64;
    loop {
        let mut posted = POSTED.lock();
        let wave = loop {
            if posted.serial != joined
                && let Some(wave) = posted.wave.clone()
            {
                joined = posted.serial;
                break wave;
            }
            WAKE.wait(&mut posted);
        };
        drop(posted);
        if !wave.take_seat() {
            continue;
        }
        ACTIVE.set(Some(Active {
            firing: None,
            run_id: wave.run_id,
            epoch: wave.epoch,
            waiters: Arc::clone(&wave.waiters),
            out: Outbox::default(),
        }));
        wave.work();
        ACTIVE.set(None);
    }
}

/// Fire `entries`, one wave, on `workers` threads, and merge what the fires
/// produced into `frontier` and the input queue in wave order. Returns the
/// first panic in wave order.
///
/// With one worker the wave fires on a pool thread while the holder waits, so
/// every callback runs on a thread that is not the writer's. With more, the
/// holder is one of the workers.
pub(super) fn fire(
    entries: Vec<Entry>,
    workers: usize,
    frontier: &mut Frontier,
) -> Option<Box<dyn Any + Send>> {
    PARALLEL_WAVES.fetch_add(1, Ordering::Relaxed);
    let len = entries.len();
    let (chunk, helpers) = if workers == 1 {
        (len, 1)
    } else {
        let chunk = len
            .checked_div(workers.saturating_mul(CHUNKS_PER_WORKER))
            .unwrap_or(1)
            .clamp(1, MAX_CHUNK);
        let chunks = len.div_ceil(chunk);
        (chunk, workers.min(chunks).saturating_sub(1))
    };
    let (run_id, epoch, waiters) = ACTIVE.with_borrow(|active| {
        active.as_ref().map_or_else(
            || (0, None, Arc::default()),
            |active| (active.run_id, active.epoch, Arc::clone(&active.waiters)),
        )
    });
    let wave = Arc::new(Wave {
        entries,
        run_id,
        epoch,
        waiters,
        chunk,
        next: AtomicUsize::new(0),
        seats: AtomicUsize::new(helpers),
        unfired: AtomicUsize::new(len),
        stretches: Mutex::new(Vec::new()),
        done: Mutex::new(()),
        finished: Condvar::new(),
    });
    let offered = post(&wave, helpers);
    if workers > 1 || !offered {
        wave.work();
    }
    wave.wait();
    POSTED.lock().wave = None;

    let mut stretches = std::mem::take(&mut *wave.stretches.lock());
    stretches.sort_unstable_by_key(|stretch| stretch.start);
    let mut first_panic = None;
    for stretch in stretches {
        frontier.merge(stretch.out);
        if first_panic.is_none() {
            first_panic = stretch.panic;
        }
    }
    first_panic
}
