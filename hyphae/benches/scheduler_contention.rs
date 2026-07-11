//! Cross-thread contention bench for the scheduler (`--features scheduler`).
//!
//! `scheduler.rs`'s microbench (`diamond/batched`) measures one thread
//! calling `batch()` repeatedly — it validates the per-call overhead of the
//! lock (see the doc comment there) but exercises zero real contention: only
//! one thread is ever touching `TICK`, so the mutex's fast (uncontended) path
//! is all that gets measured. The whole point of making the tick queue
//! process-wide instead of thread-local is to make `batch()` safe when
//! MULTIPLE threads touch it concurrently (a timer thread and a
//! command-handling thread converging on a shared cell was the actual
//! production bug). This file measures that case directly, at increasing
//! thread counts, so a regression in the contended path — not just the
//! uncontended one — shows up in CI/local bench runs instead of only in
//! production under real load.
//!
//! Two shapes, because they exercise different parts of the cross-thread
//! protocol:
//!
//! - `contention/independent` — N threads, each batching its OWN, unrelated
//!   diamond (no shared cells). Isolates pure lock contention on `TICK`
//!   (every thread's `enqueue`/`batch` call still serializes through the same
//!   mutex, even though the graphs never interact) from any drain-ownership
//!   behavior, since each thread's own `batch()` call is always "outermost"
//!   for its own ops and there's nothing for another thread to wait on.
//! - `contention/shared_sink` — N threads, each with its own source cell, all
//!   feeding ONE shared `join_vec` sink. This is the actual bug shape: only
//!   one thread's `batch()` call can be "the drainer" for a given active
//!   window, so the others correctly join and wait — exercising the
//!   `Condvar` wait/wake handshake under real concurrent load, not just the
//!   lock.
//!
//! Both report `Throughput::Elements` (total `batch()` calls across all
//! threads) so criterion prints ops/sec — the metric that actually answers
//! "does contention degrade throughput as threads increase," which raw
//! per-sample wall-clock time doesn't make legible on its own.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use hyphae::{
    Cell, CellMutable, MapExt, MaterializeDefinite, Mutable, SubscriptionGuard, Watchable, batch,
    join_vec,
};

/// How many `batch()` calls each thread performs per sample. High enough
/// that the fixed per-sample setup (spawning the scope, building the graphs)
/// is a small fraction of what's measured; low enough that a full sweep over
/// every thread count still runs in a reasonable amount of local/CI time.
const OPS_PER_THREAD: u64 = 20_000;

const THREAD_COUNTS: [usize; 4] = [1, 2, 4, 8];

/// Time `n_threads` running `body` concurrently (via `thread::scope`, so no
/// per-sample thread-spawn cost pollutes anything outside the timed region —
/// `criterion`'s `iter_custom` lets us control exactly what's inside the
/// clock). `body(thread_index)` runs once per thread for the whole sample;
/// it's expected to loop `OPS_PER_THREAD` times internally.
fn time_concurrent(n_threads: usize, body: impl Fn(usize) + Sync) -> Duration {
    let start = Instant::now();
    thread::scope(|s| {
        for t in 0..n_threads {
            let body = &body;
            s.spawn(move || body(t));
        }
    });
    start.elapsed()
}

fn bench_contention_independent(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention/independent");

    for &n_threads in &THREAD_COUNTS {
        group.throughput(Throughput::Elements(n_threads as u64 * OPS_PER_THREAD));
        group.bench_function(format!("threads={n_threads}"), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    // Fresh, wholly independent diamond per thread per sample —
                    // no shared cells, so the only thing threads contend on is
                    // `TICK`'s mutex itself.
                    let sources: Vec<Cell<i64, CellMutable>> =
                        (0..n_threads).map(|_| Cell::new(0i64)).collect();
                    let guards: Vec<SubscriptionGuard> = sources
                        .iter()
                        .map(|s| s.clone().map(|x| x + 1).materialize().subscribe(|_| {}))
                        .collect();

                    total += time_concurrent(n_threads, |t| {
                        let s = &sources[t];
                        for i in 0..OPS_PER_THREAD {
                            batch(|| s.set(black_box(i as i64)));
                        }
                    });

                    drop(guards);
                }
                total
            });
        });
    }

    group.finish();
}

fn bench_contention_shared_sink(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention/shared_sink");

    for &n_threads in &THREAD_COUNTS {
        group.throughput(Throughput::Elements(n_threads as u64 * OPS_PER_THREAD));
        group.bench_function(format!("threads={n_threads}"), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    // One source per thread, but ALL of them feed a single
                    // shared sink — the actual bug shape (e.g. a timer thread
                    // and a command-handling thread both notifying into one
                    // downstream subgraph). Only one thread's `batch()` call
                    // can be the drainer for a given active window; the rest
                    // correctly join and wait, so this exercises the
                    // `Condvar` handshake, not just the mutex.
                    let sources: Vec<Cell<i64, CellMutable>> =
                        (0..n_threads).map(|_| Cell::new(0i64)).collect();
                    let solves = Arc::new(AtomicU64::new(0));
                    let counter = solves.clone();
                    let sink = join_vec(sources.iter().cloned().map(|s| s.lock()).collect())
                        .map(move |v: &Vec<i64>| {
                            counter.fetch_add(1, Ordering::Relaxed);
                            v.iter().sum::<i64>()
                        })
                        .materialize();
                    let guard = sink.subscribe(|_| {});

                    total += time_concurrent(n_threads, |t| {
                        let s = &sources[t];
                        for i in 0..OPS_PER_THREAD {
                            batch(|| s.set(black_box(i as i64)));
                        }
                    });

                    black_box(solves.load(Ordering::Relaxed));
                    drop(guard);
                }
                total
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_contention_independent,
    bench_contention_shared_sink
);
criterion_main!(benches);
