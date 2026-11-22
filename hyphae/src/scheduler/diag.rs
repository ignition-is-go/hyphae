//! DIAGNOSTIC (not for merge). Opt-in via env, zero work when unset.
//!
//! `HYPHAE_DRAIN_TRACE=<file>`: one line per drain stint and per outermost
//! batch close, wall-clock microseconds since the Unix epoch, written by a
//! background thread:
//!   `D tid start_us end_us waves ops max_groups par_waves handoff`
//!   `B tid open_us close_us claimed draining_at_close depth_after`
//! `HYPHAE_CALL_SITES=<file>` with `HYPHAE_CALL_SITES_EVERY=N` (default 2000):
//! stack of 1 in N deferred enqueues.
//! Each process appends to `<file>.<pid>`.
use std::{
    backtrace::Backtrace,
    io::Write,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{Sender, channel},
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

fn tid() -> u64 {
    thread_local! { static TID: u64 = {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }; }
    TID.with(|t| *t)
}

type Line = Box<dyn FnOnce() -> String + Send>;

fn writer(path: String) -> Mutex<Sender<Line>> {
    let path = format!("{path}.{}", std::process::id());
    let (tx, rx) = channel::<Line>();
    std::thread::Builder::new()
        .name("hyphae-diag".into())
        .spawn(move || {
            let Ok(file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            else {
                return;
            };
            let mut file = std::io::BufWriter::with_capacity(1 << 20, file);
            for line in rx {
                let _ = file.write_all(line().as_bytes());
                let _ = file.flush();
            }
        })
        .expect("spawn hyphae diag writer");
    Mutex::new(tx)
}

static TRACE: LazyLock<Option<Mutex<Sender<Line>>>> =
    LazyLock::new(|| std::env::var("HYPHAE_DRAIN_TRACE").ok().map(writer));

pub(super) fn tracing() -> bool {
    TRACE.is_some()
}

fn emit(tx: &Mutex<Sender<Line>>, line: String) {
    emit_lazy(tx, Box::new(move || line));
}

fn emit_lazy(tx: &Mutex<Sender<Line>>, line: Line) {
    let tx = tx
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let _ = tx.send(line);
}

pub(super) struct Stint {
    pub start: u64,
    pub waves: u64,
    pub ops: u64,
    pub max_groups: u64,
    pub par_waves: u64,
}

impl Stint {
    pub fn begin() -> Option<Self> {
        tracing().then(|| Self {
            start: now_us(),
            waves: 0,
            ops: 0,
            max_groups: 0,
            par_waves: 0,
        })
    }
    pub fn wave(&mut self, groups: usize, ops: usize, parallel: bool) {
        self.waves += 1;
        self.ops += ops as u64;
        self.max_groups = self.max_groups.max(groups as u64);
        self.par_waves += u64::from(parallel);
    }
    pub fn end(self, handoff: bool) {
        if let Some(tx) = TRACE.as_ref() {
            emit(
                tx,
                format!(
                    "D {} {} {} {} {} {} {} {}\n",
                    tid(),
                    self.start,
                    now_us(),
                    self.waves,
                    self.ops,
                    self.max_groups,
                    self.par_waves,
                    u8::from(handoff)
                ),
            );
        }
    }
}

pub(super) fn batch_close(opened: u64, claimed: bool, draining: bool, depth: u32) {
    if let Some(tx) = TRACE.as_ref() {
        emit(
            tx,
            format!(
                "B {} {} {} {} {} {}\n",
                tid(),
                opened,
                now_us(),
                u8::from(claimed),
                u8::from(draining),
                depth
            ),
        );
    }
}

struct Sample {
    deps: usize,
    height: u64,
    in_drain: bool,
    coalesced: bool,
    seq: u64,
    stack: Backtrace,
}

static EVERY: LazyLock<Option<u64>> = LazyLock::new(|| {
    std::env::var("HYPHAE_CALL_SITES").ok()?;
    Some(super::env_usize("HYPHAE_CALL_SITES_EVERY").map_or(2000, |n| n.max(1) as u64))
});
static IN_DRAIN: AtomicU64 = AtomicU64::new(0);
static EXTERNAL: AtomicU64 = AtomicU64::new(0);
static SITES: LazyLock<Mutex<Sender<Line>>> =
    LazyLock::new(|| writer(std::env::var("HYPHAE_CALL_SITES").unwrap_or_default()));

pub(super) fn sample(node: &dyn super::DepNode, height: u64, in_drain: bool, coalesced: bool) {
    let Some(every) = *EVERY else { return };
    let counter = if in_drain { &IN_DRAIN } else { &EXTERNAL };
    let seq = counter.fetch_add(1, Ordering::Relaxed);
    if seq % every != 0 {
        return;
    }
    let s = Sample {
        deps: node.deps().len(),
        height,
        in_drain,
        coalesced,
        seq,
        stack: Backtrace::force_capture(),
    };
    let now = now_us();
    emit_lazy(
        &SITES,
        Box::new(move || {
            format!(
                "=== t_us={now} deps={} height={} in_drain={} coalesced={} seq={}\n{}\n",
                s.deps, s.height, s.in_drain, s.coalesced, s.seq, s.stack
            )
        }),
    );
}

/// `HYPHAE_OP_WAITS=<file>`: every 100 ms, per height, the deferred ops run, their
/// summed and longest wait from (latest) enqueue to start, and their summed run time:
///   `W t_us height:count/wait_sum_us/wait_max_us/run_sum_us ... L site:count/wait_sum_us/wait_max_us ...`
static WAITS: LazyLock<Option<Mutex<Sender<Line>>>> =
    LazyLock::new(|| std::env::var("HYPHAE_OP_WAITS").ok().map(writer));
const HEIGHTS: usize = 64;
static W_COUNT: [AtomicU64; HEIGHTS] = [const { AtomicU64::new(0) }; HEIGHTS];
static W_SUM: [AtomicU64; HEIGHTS] = [const { AtomicU64::new(0) }; HEIGHTS];
static W_MAX: [AtomicU64; HEIGHTS] = [const { AtomicU64::new(0) }; HEIGHTS];
static W_RUN: [AtomicU64; HEIGHTS] = [const { AtomicU64::new(0) }; HEIGHTS];
static W_REPORTED: AtomicU64 = AtomicU64::new(0);
static L_COUNT: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static L_SUM: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static L_MAX: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

pub(super) fn waits_on() -> bool {
    WAITS.is_some()
}

/// Tick-lock acquisition wait by site: 0 enqueue, 1 drain pop, 2 batch open, 3 batch close.
pub(super) fn lock_wait(site: usize, waited: std::time::Duration) {
    let us = u64::try_from(waited.as_nanos() / 1000).unwrap_or(u64::MAX);
    L_COUNT[site].fetch_add(1, Ordering::Relaxed);
    L_SUM[site].fetch_add(u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX), Ordering::Relaxed);
    L_MAX[site].fetch_max(us, Ordering::Relaxed);
}

pub(super) fn wrap(height: u64, run: Box<dyn FnOnce() + Send>) -> Box<dyn FnOnce() + Send> {
    if WAITS.is_none() {
        return run;
    }
    let queued = std::time::Instant::now();
    Box::new(move || {
        let started = std::time::Instant::now();
        run();
        let slot = usize::try_from(height).unwrap_or(HEIGHTS - 1).min(HEIGHTS - 1);
        let wait = u64::try_from(started.duration_since(queued).as_micros()).unwrap_or(u64::MAX);
        let ran = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        W_COUNT[slot].fetch_add(1, Ordering::Relaxed);
        W_SUM[slot].fetch_add(wait, Ordering::Relaxed);
        W_MAX[slot].fetch_max(wait, Ordering::Relaxed);
        W_RUN[slot].fetch_add(ran, Ordering::Relaxed);
        let now = now_us();
        let last = W_REPORTED.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= 100_000
            && W_REPORTED.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
            && let Some(tx) = WAITS.as_ref()
        {
            let mut line = format!("W {now}");
            for h in 0..HEIGHTS {
                let c = W_COUNT[h].swap(0, Ordering::Relaxed);
                let s = W_SUM[h].swap(0, Ordering::Relaxed);
                let m = W_MAX[h].swap(0, Ordering::Relaxed);
                let r = W_RUN[h].swap(0, Ordering::Relaxed);
                if c > 0 {
                    line.push_str(&format!(" {h}:{c}/{s}/{m}/{r}"));
                }
            }
            line.push_str(" L");
            for site in 0..4 {
                let c = L_COUNT[site].swap(0, Ordering::Relaxed);
                let ns = L_SUM[site].swap(0, Ordering::Relaxed);
                let m = L_MAX[site].swap(0, Ordering::Relaxed);
                line.push_str(&format!(" {site}:{c}/{}/{m}", ns / 1000));
            }
            line.push('\n');
            emit(tx, line);
        }
    })
}
