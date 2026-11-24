//! Scheduling-domain benches.
//!
//! - `cross_domain_delay/*` — one graph is parked inside a 200 ms wave on
//!   another thread while this thread batches a write to an unrelated cell and
//!   waits for the propagation to reach a subscriber. `same_root` puts both
//!   graphs in the root domain, where they share one tick: the write waits for
//!   the rest of the long wave. The other cases put the graphs in different
//!   domains. The sample is the time from the write to the observation, so the
//!   200 ms wave itself is never inside the clock.
//! - `root_hot_path_with_children/*` — the `scheduler` bench's root-only
//!   shapes, run in a process that has a child domain. Compare against the
//!   same names in `scheduler`, which runs with no child domain, to see what a
//!   root-only graph pays once another part of the program uses domains.
//! - `fan_out/*` — one root write read by `N` child domains, each through one
//!   hand-off subscription feeding one child cell.
//!   `root_write_return` times only the root write, which queues `N`
//!   deliveries and wakes `N` workers. `until_all_children_applied` adds the
//!   wait for every child to apply it. `same_domain_reference` is the same
//!   `N`-way fan-out with every reader in root, the cost without a boundary.
//!   Each iteration waits for the children before the next write, so the
//!   hand-off queues never build up inside the clock.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hyphae::{
    Cell, CellImmutable, CellMutable, Gettable, JoinExt, MapExt, Materialize, Mutable, Signal,
    SubscriptionGuard, Watchable, batch, scheduler::Domain,
};

const LONG_WAVE: Duration = Duration::from_millis(200);

/// A source whose derived cell takes [`LONG_WAVE`] to recompute, and a flag
/// raised once that recompute is running.
struct SlowGraph {
    source: Cell<i64, CellMutable>,
    running: Arc<AtomicBool>,
    _derived: Cell<i64, CellImmutable>,
}

fn slow_graph() -> SlowGraph {
    let source = Cell::new(0i64);
    let running = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&running);
    let derived = source
        .clone()
        .map(move |value| {
            if *value != 0 {
                flag.store(true, Ordering::SeqCst);
                thread::sleep(LONG_WAVE);
            }
            *value
        })
        .materialize();
    SlowGraph {
        source,
        running,
        _derived: derived,
    }
}

/// A source, a derived cell and the last value a subscriber of it observed.
struct FastGraph {
    source: Cell<i64, CellMutable>,
    observed: Arc<AtomicI64>,
    _guard: SubscriptionGuard,
}

fn fast_graph() -> FastGraph {
    let source = Cell::new(0i64);
    let observed = Arc::new(AtomicI64::new(0));
    let sink = Arc::clone(&observed);
    let derived = source.clone().map(|value| *value).materialize();
    let guard = derived.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            sink.store(**value, Ordering::SeqCst);
        }
    });
    FastGraph {
        source,
        observed,
        _guard: guard,
    }
}

/// Time from a batched write to `fast` until its subscriber observes it, taken
/// while `slow` is inside its long wave on another thread.
fn delay_while_busy(
    slow_domain: &Domain,
    slow: &SlowGraph,
    fast_domain: &Domain,
    fast: &FastGraph,
    iters: u64,
) -> Duration {
    let mut total = Duration::ZERO;
    for iteration in 1..=iters {
        let value = i64::try_from(iteration).unwrap_or(i64::MAX);
        slow.running.store(false, Ordering::SeqCst);
        let elapsed = thread::scope(|scope| {
            scope.spawn(|| slow_domain.batch(|| slow.source.set(value)));
            while !slow.running.load(Ordering::SeqCst) {
                std::hint::spin_loop();
            }
            let start = Instant::now();
            fast_domain.batch(|| fast.source.set(value));
            while fast.observed.load(Ordering::SeqCst) != value {
                std::hint::spin_loop();
            }
            start.elapsed()
        });
        total = total.saturating_add(elapsed);
    }
    total
}

fn bench_cross_domain_delay(c: &mut Criterion) {
    let mut group = c.benchmark_group("cross_domain_delay");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(5));

    let root = Domain::root();
    let child_a = Domain::child();
    let child_b = Domain::child();
    let cases = [
        ("same_root", &root, &root),
        ("sibling_children", &child_a, &child_b),
        ("busy_child_root_write", &child_a, &root),
        ("busy_root_child_write", &root, &child_b),
    ];
    for (name, slow_domain, fast_domain) in cases {
        let slow = slow_domain.enter(slow_graph);
        let fast = fast_domain.enter(fast_graph);
        group.bench_function(name, |b| {
            b.iter_custom(|iters| delay_while_busy(slow_domain, &slow, fast_domain, &fast, iters));
        });
    }
    group.finish();
}

struct Diamond {
    source: Cell<i64, CellMutable>,
    solves: Arc<AtomicU64>,
    _guard: SubscriptionGuard,
}

fn build_diamond() -> Diamond {
    let source = Cell::new(0i64);
    let add_leg = source.clone().map(|x| x.saturating_add(1)).materialize();
    let multiply_leg = source.clone().map(|x| x.saturating_mul(10)).materialize();
    let solves = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&solves);
    let sink = add_leg
        .join(multiply_leg)
        .map(move |(x, y)| {
            counter.fetch_add(1, Ordering::Relaxed);
            black_box(x.wrapping_add(*y))
        })
        .materialize();
    let guard = sink.subscribe(|_| {});
    Diamond {
        source,
        solves,
        _guard: guard,
    }
}

fn bench_root_hot_path_with_children(c: &mut Criterion) {
    let _child = Domain::child();
    let mut group = c.benchmark_group("root_hot_path_with_children");

    group.bench_function("single_cell/synchronous", |b| {
        let cell = Cell::new(0i64);
        let _guard = cell.subscribe(|signal| {
            black_box(signal);
        });
        let mut value = 0i64;
        b.iter(|| {
            value = value.wrapping_add(1);
            cell.set(black_box(value));
        });
    });

    let diamond = build_diamond();
    group.bench_function("diamond/synchronous", |b| {
        let mut value = 0i64;
        b.iter(|| {
            value = value.wrapping_add(1);
            diamond.source.set(black_box(value));
        });
    });
    group.bench_function("diamond/batched", |b| {
        let mut value = 0i64;
        b.iter(|| {
            value = value.wrapping_add(1);
            batch(|| diamond.source.set(black_box(value)));
        });
    });
    black_box(diamond.solves.load(Ordering::Relaxed));
    group.finish();
}

/// `width` readers of one root cell, each a derived cell in its own child
/// domain, or all in root for the reference.
struct FanOut {
    source: Cell<i64, CellMutable>,
    children: Vec<Domain>,
    readers: Vec<Cell<i64, CellImmutable>>,
}

impl FanOut {
    /// Every reader holds `value`: the timed writes all reached all of them.
    fn assert_applied(&self, value: i64) {
        for child in &self.children {
            child.flush();
        }
        assert!(self.readers.iter().all(|reader| reader.get() == value));
    }
}

fn fan_out(width: usize, across_children: bool) -> FanOut {
    let source = Cell::new(0i64);
    let mut children = Vec::new();
    let mut readers = Vec::new();
    for _ in 0..width {
        let domain = if across_children {
            Domain::child()
        } else {
            Domain::root()
        };
        readers.push(domain.enter(|| source.clone().map(|value| *value).materialize()));
        if across_children {
            children.push(domain);
        }
    }
    FanOut {
        source,
        children,
        readers,
    }
}

fn bench_fan_out(c: &mut Criterion) {
    let mut group = c.benchmark_group("fan_out");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(5));

    for width in [1usize, 8, 64] {
        let reference = fan_out(width, false);
        let mut reference_value = 0i64;
        group.bench_function(BenchmarkId::new("same_domain_reference", width), |b| {
            b.iter(|| {
                reference_value = reference_value.wrapping_add(1);
                reference.source.set(black_box(reference_value));
            });
        });
        reference.assert_applied(reference_value);
        drop(reference);

        let graph = fan_out(width, true);
        let mut value = 0i64;
        group.bench_function(BenchmarkId::new("root_write_return", width), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    value = value.wrapping_add(1);
                    let start = Instant::now();
                    graph.source.set(black_box(value));
                    total = total.saturating_add(start.elapsed());
                    for child in &graph.children {
                        child.flush();
                    }
                }
                total
            });
        });
        group.bench_function(BenchmarkId::new("until_all_children_applied", width), |b| {
            b.iter_custom(|iters| {
                let start = Instant::now();
                for _ in 0..iters {
                    value = value.wrapping_add(1);
                    graph.source.set(black_box(value));
                    for child in &graph.children {
                        child.flush();
                    }
                }
                start.elapsed()
            });
        });
        graph.assert_applied(value);
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_cross_domain_delay,
    bench_root_hot_path_with_children,
    bench_fan_out
);
criterion_main!(benches);
