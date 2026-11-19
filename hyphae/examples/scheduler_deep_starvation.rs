//! Does a saturated tick queue starve deep cells while shallow cells keep up?
//!
//! One timer thread writes `runners` source cells at 30 Hz. Event threads write
//! their own cells at a paced rate. Every map costs `cost_us` of CPU. Two sinks
//! hang off runner 0: one after the first map, one after `deep_depth` more.
//! `batched` wraps every write in `batch()`, so all propagation goes through the
//! process-wide queue; `eager` runs the same graph and writes with no batch.
//!
//! Prints one JSON line per run.

use std::{
    env,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use hyphae::{
    Cell, CellImmutable, CellMutable, MapExt, Materialize, Mutable, Signal, SubscriptionGuard,
    Watchable, batch,
};

const TICK_HZ: f64 = 30.0;
const CELLS_PER_EVENT_THREAD: usize = 8;

struct Config {
    batched: bool,
    runners: usize,
    shallow_depth: usize,
    deep_depth: usize,
    cost: Duration,
    event_threads: usize,
    events_per_sec: f64,
    event_depth: usize,
    seconds: f64,
}

fn arg<T: std::str::FromStr>(name: &str, default: T) -> T {
    let prefix = format!("--{name}=");
    env::args()
        .find_map(|a| a.strip_prefix(&prefix).and_then(|v| v.parse().ok()))
        .unwrap_or(default)
}

fn spin(cost: Duration, ops: &AtomicU64) {
    let start = Instant::now();
    while start.elapsed() < cost {
        std::hint::spin_loop();
    }
    ops.fetch_add(1, Ordering::Relaxed);
}

fn chain(
    source: Cell<u64, CellImmutable>,
    depth: usize,
    cost: Duration,
    ops: &Arc<AtomicU64>,
) -> Cell<u64, CellImmutable> {
    (0..depth).fold(source, |cell, _| {
        let ops = ops.clone();
        cell.map(move |value| {
            spin(cost, &ops);
            *value
        })
        .materialize()
    })
}

type Arrivals = Arc<Mutex<Vec<(u64, Instant)>>>;

fn record(cell: &Cell<u64, CellImmutable>) -> (Arrivals, SubscriptionGuard) {
    let arrivals: Arrivals = Arc::new(Mutex::new(Vec::new()));
    let sink = arrivals.clone();
    let guard = cell.subscribe(move |signal| {
        if let Signal::Value(tick) = signal
            && **tick > 0
        {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((**tick, Instant::now()));
        }
    });
    (arrivals, guard)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (p * (sorted.len() - 1) as f64).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

struct SinkStats {
    per_sec: f64,
    gap_p50_ms: f64,
    gap_p99_ms: f64,
    gap_max_ms: f64,
    latency_p50_ms: f64,
    latency_p99_ms: f64,
    max_ticks_merged: u64,
    out_of_order: u64,
}

fn sink_stats(arrivals: &Arrivals, emitted: &[Instant], seconds: f64) -> SinkStats {
    let arrivals = arrivals
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut gaps: Vec<f64> = arrivals
        .windows(2)
        .map(|w| w[1].1.duration_since(w[0].1).as_secs_f64() * 1e3)
        .collect();
    gaps.sort_by(f64::total_cmp);
    let mut latencies: Vec<f64> = arrivals
        .iter()
        .filter_map(|(tick, at)| {
            emitted
                .get(usize::try_from(*tick).ok()?.checked_sub(1)?)
                .map(|sent| at.saturating_duration_since(*sent).as_secs_f64() * 1e3)
        })
        .collect();
    latencies.sort_by(f64::total_cmp);
    SinkStats {
        per_sec: arrivals.len() as f64 / seconds,
        gap_p50_ms: percentile(&gaps, 0.5),
        gap_p99_ms: percentile(&gaps, 0.99),
        gap_max_ms: gaps.last().copied().unwrap_or(f64::NAN),
        latency_p50_ms: percentile(&latencies, 0.5),
        latency_p99_ms: percentile(&latencies, 0.99),
        max_ticks_merged: arrivals
            .windows(2)
            .map(|w| w[1].0.saturating_sub(w[0].0))
            .max()
            .unwrap_or(0),
        out_of_order: arrivals.windows(2).filter(|w| w[1].0 <= w[0].0).count() as u64,
    }
}

fn process_cpu_secs() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .map_or(Vec::new(), |(_, rest)| rest.split_whitespace().collect());
    let ticks = |i: usize| {
        fields
            .get(i)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    (ticks(11) + ticks(12)) / 100.0
}

fn write(batched: bool, f: impl FnOnce()) {
    if batched { batch(f) } else { f() }
}

fn main() {
    let config = Config {
        batched: arg("mode", String::from("batched")) == "batched",
        runners: arg("runners", 600),
        shallow_depth: arg("shallow-depth", 4),
        deep_depth: arg("deep-depth", 40),
        cost: Duration::from_nanos(arg("cost-ns", 5_000)),
        event_threads: arg("event-threads", 8),
        events_per_sec: arg("events-per-sec", 0.0),
        event_depth: arg("event-depth", 20),
        seconds: arg("seconds", 10.0),
    };
    let ops = Arc::new(AtomicU64::new(0));

    let runners: Vec<Cell<u64, CellMutable>> = (0..config.runners.max(1))
        .map(|_| Cell::new(0u64))
        .collect();
    let mut guards: Vec<SubscriptionGuard> = Vec::new();
    let mut probe = None;
    for (index, runner) in runners.iter().enumerate() {
        let first = chain(runner.clone().lock(), 1, config.cost, &ops);
        let shallow = chain(
            first.clone(),
            config.shallow_depth.saturating_sub(1),
            config.cost,
            &ops,
        );
        if index == 0 {
            let deep = chain(shallow.clone(), config.deep_depth, config.cost, &ops);
            probe = Some((record(&first), record(&deep)));
        }
        guards.push(shallow.subscribe(|_| {}));
    }
    let Some(((shallow_arrivals, _shallow_guard), (deep_arrivals, _deep_guard))) = probe else {
        return;
    };

    let event_cells: Vec<Vec<Cell<u64, CellMutable>>> = (0..config.event_threads)
        .map(|_| {
            (0..CELLS_PER_EVENT_THREAD)
                .map(|_| {
                    let cell = Cell::new(0u64);
                    guards.push(
                        chain(cell.clone().lock(), config.event_depth, config.cost, &ops)
                            .subscribe(|_| {}),
                    );
                    cell
                })
                .collect()
        })
        .collect();

    let stop = Arc::new(AtomicBool::new(false));
    let events_sent = Arc::new(AtomicU64::new(0));
    let cpu_before = process_cpu_secs();
    let ops_before = ops.load(Ordering::Relaxed);
    let started = Instant::now();

    let event_handles: Vec<_> = event_cells
        .into_iter()
        .map(|cells| {
            let stop = stop.clone();
            let sent = events_sent.clone();
            let batched = config.batched;
            let per_thread = config.events_per_sec / config.event_threads.max(1) as f64;
            thread::spawn(move || {
                if per_thread <= 0.0 {
                    return;
                }
                let period = Duration::from_secs_f64(1.0 / per_thread);
                let mut next = Instant::now();
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    n = n.saturating_add(1);
                    if let Some(cell) = cells.get(usize::try_from(n).unwrap_or(0) % cells.len()) {
                        write(batched, || cell.set(n));
                    }
                    sent.fetch_add(1, Ordering::Relaxed);
                    next = next.checked_add(period).unwrap_or(next);
                    let now = Instant::now();
                    if next > now {
                        thread::sleep(next.duration_since(now));
                    } else {
                        next = now;
                    }
                }
            })
        })
        .collect();

    // Fixed-rate with missed ticks folded, like the timer reactor.
    let period = Duration::from_secs_f64(1.0 / TICK_HZ);
    let mut emitted: Vec<Instant> = Vec::new();
    let mut ticks_skipped = 0u64;
    let mut next = Instant::now();
    while started.elapsed().as_secs_f64() < config.seconds {
        let tick = u64::try_from(emitted.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        emitted.push(Instant::now());
        write(config.batched, || {
            for runner in &runners {
                runner.set(tick);
            }
        });
        next = next.checked_add(period).unwrap_or(next);
        let now = Instant::now();
        if next > now {
            thread::sleep(next.duration_since(now));
        } else {
            let missed = now.duration_since(next).as_secs_f64() * TICK_HZ;
            ticks_skipped = ticks_skipped.saturating_add(missed as u64);
            next = now.checked_add(period).unwrap_or(now);
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    for handle in event_handles {
        let _ = handle.join();
    }

    let shallow = sink_stats(&shallow_arrivals, &emitted, seconds);
    let deep = sink_stats(&deep_arrivals, &emitted, seconds);
    let ops_done = ops.load(Ordering::Relaxed).saturating_sub(ops_before);
    let work_cores = ops_done as f64 * config.cost.as_secs_f64() / seconds;
    println!(
        "{{\"mode\":\"{}\",\"runners\":{},\"events_per_sec_target\":{},\"events_per_sec\":{:.0},\
\"ticks_per_sec\":{:.1},\"ticks_skipped\":{},\"map_calls_per_sec\":{:.0},\"work_cores\":{:.2},\
\"process_cores\":{:.2},\"shallow_per_sec\":{:.1},\"shallow_gap_max_ms\":{:.0},\
\"deep_per_sec\":{:.1},\"deep_gap_p50_ms\":{:.0},\"deep_gap_p99_ms\":{:.0},\"deep_gap_max_ms\":{:.0},\
\"deep_latency_p50_ms\":{:.0},\"deep_latency_p99_ms\":{:.0},\"deep_max_ticks_merged\":{},\
\"out_of_order\":{}}}",
        if config.batched { "batched" } else { "eager" },
        config.runners,
        config.events_per_sec,
        events_sent.load(Ordering::Relaxed) as f64 / seconds,
        emitted.len() as f64 / seconds,
        ticks_skipped,
        ops_done as f64 / seconds,
        work_cores,
        (process_cpu_secs() - cpu_before) / seconds,
        shallow.per_sec,
        shallow.gap_max_ms,
        deep.per_sec,
        deep.gap_p50_ms,
        deep.gap_p99_ms,
        deep.gap_max_ms,
        deep.latency_p50_ms,
        deep.latency_p99_ms,
        deep.max_ticks_merged,
        shallow.out_of_order.saturating_add(deep.out_of_order),
    );
    drop(guards);
}
