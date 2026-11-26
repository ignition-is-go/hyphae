//! Per-emission bookkeeping harness.
//!
//! Each case builds a graph once, warms it, then times `REPEATS` blocks of
//! `ops` operations. It prints one JSON object per case on stdout with
//! allocations per operation (counted by a global allocator, the primary and
//! deterministic metric), nanoseconds per operation for every repeat, the work
//! the graph did per operation (subscriber deliveries, closure calls) and an
//! error count. `tools/emission-bench.py` runs this binary several times,
//! aggregates the runs and compares two result files.
//!
//! The counting allocator adds an atomic increment to every allocation, so
//! absolute times are slightly higher than in an uninstrumented build. Both
//! sides of a comparison pay it.
//!
//! Usage: `cargo bench -p hyphae --bench emission -- [--list] [filter...]`.

#![recursion_limit = "256"]
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::indexing_slicing
)]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::Instant,
};

use hyphae::{
    Cell, CellImmutable, CellMap, CellMutable, FilterExt, MapDiff, MapExt, Materialize, Mutable,
    Signal, SubscriptionGuard, SwitchMapExt, TapExt, TryMapExt, Watchable, batch, join_vec,
    scheduler::Domain,
};
use seq_macro::seq;

struct CountingAllocator;

static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(pointer, old, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

const REPEATS: usize = 7;
const WARMUP_OPS: u64 = 2_000;

/// What a case does per operation. `run(first, count)` performs operations
/// `first..first + count`; the case decides what one operation means.
/// `errors()` returns how many observations disagreed with what the
/// operations should have produced so far.
struct Bench {
    run: Box<dyn FnMut(u64, u64)>,
    work: Arc<AtomicU64>,
    errors: Box<dyn Fn() -> u64>,
    _keep: Box<dyn std::any::Any>,
}

struct Case {
    name: String,
    covers: &'static str,
    ops: u64,
    build: Box<dyn Fn() -> Bench>,
}

fn case(
    name: impl Into<String>,
    covers: &'static str,
    ops: u64,
    build: impl Fn() -> Bench + 'static,
) -> Case {
    Case {
        name: name.into(),
        covers,
        ops,
        build: Box::new(build),
    }
}

/// A subscriber that counts deliveries and remembers the last value.
fn sink(
    cell: &Cell<u64, CellImmutable>,
    work: &Arc<AtomicU64>,
) -> (SubscriptionGuard, Arc<AtomicU64>) {
    let last = Arc::new(AtomicU64::new(u64::MAX));
    let (w, l) = (Arc::clone(work), Arc::clone(&last));
    let guard = cell.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            w.fetch_add(1, Ordering::Relaxed);
            l.store(**value, Ordering::Relaxed);
        }
    });
    (guard, last)
}

/// Errors are the operations whose final value the sink did not see: after a
/// block ending at `expected`, `last` must equal it.
fn expect_last(last: Arc<AtomicU64>, expected: Arc<AtomicU64>) -> Box<dyn Fn() -> u64> {
    Box::new(move || u64::from(last.load(Ordering::Relaxed) != expected.load(Ordering::Relaxed)))
}

/// `source -> map(+1) x depth (each materialized) -> subscriber`, one batched
/// write per operation, optionally inside a child domain.
fn cell_chain(depth: usize, domain: Option<Domain>) -> Bench {
    let domain = domain.unwrap_or_else(Domain::root);
    let work = Arc::new(AtomicU64::new(0));
    let (source, cells, guard, last) = domain.enter(|| {
        let source = Cell::new(0u64);
        let mut cells = vec![source.clone().map(|x| x.wrapping_add(1)).materialize()];
        for _ in 1..depth {
            let next = cells[cells.len() - 1]
                .clone()
                .map(|x| x.wrapping_add(1))
                .materialize();
            cells.push(next);
        }
        let (guard, last) = sink(&cells[cells.len() - 1], &work);
        (source, cells, guard, last)
    });
    let expected = Arc::new(AtomicU64::new(0));
    let exp = Arc::clone(&expected);
    let src = source.clone();
    let d = depth as u64;
    Bench {
        run: Box::new(move |first, count| {
            for i in first..first + count {
                domain.batch(|| src.set(black_box(i)));
            }
            exp.store((first + count - 1).wrapping_add(d), Ordering::Relaxed);
        }),
        work,
        errors: expect_last(last, expected),
        _keep: Box::new((source, cells, guard)),
    }
}

macro_rules! fused_chain {
    ($depth:literal, $tail:literal) => {{
        let work = Arc::new(AtomicU64::new(0));
        let source = Cell::new(0u64);
        let cell = seq!(_N in 0..$tail {
            source.clone().map(|x| x.wrapping_add(1)) #(.map(|x| x.wrapping_add(1)))*
        })
        .materialize();
        let (guard, last) = sink(&cell, &work);
        let expected = Arc::new(AtomicU64::new(0));
        let exp = Arc::clone(&expected);
        let src = source.clone();
        Bench {
            run: Box::new(move |first, count| {
                for i in first..first + count {
                    batch(|| src.set(black_box(i)));
                }
                exp.store((first + count - 1).wrapping_add($depth), Ordering::Relaxed);
            }),
            work,
            errors: expect_last(last, expected),
            _keep: Box::new((source, cell, guard)),
        }
    }};
}

/// One source read by `width` materialized cells, each with a subscriber.
fn fan_out(width: usize) -> Bench {
    let work = Arc::new(AtomicU64::new(0));
    let source = Cell::new(0u64);
    let mut keep = Vec::new();
    let mut lasts = Vec::new();
    for _ in 0..width {
        let cell = source.clone().map(|x| x.wrapping_add(1)).materialize();
        let (guard, last) = sink(&cell, &work);
        keep.push((cell, guard));
        lasts.push(last);
    }
    let expected = Arc::new(AtomicU64::new(0));
    let exp = Arc::clone(&expected);
    let src = source.clone();
    Bench {
        run: Box::new(move |first, count| {
            for i in first..first + count {
                batch(|| src.set(black_box(i)));
            }
            exp.store(first + count, Ordering::Relaxed);
        }),
        work,
        errors: Box::new(move || {
            let want = expected.load(Ordering::Relaxed);
            lasts
                .iter()
                .filter(|l| l.load(Ordering::Relaxed) != want)
                .count() as u64
        }),
        _keep: Box::new((source, keep)),
    }
}

/// `n` row writes to one `CellMap` inside one batch, observed by a diff
/// subscriber. Work counts diff callbacks; `changes` counts rows delivered,
/// which must equal `n` per operation.
fn cellmap_batch(n: u64, domain: Option<Domain>) -> Bench {
    cellmap_batch_keys(n, domain, |i, k| (k, i + 1))
}

/// [`cellmap_batch`] with operation `i`'s `k`th write storing `write(i, k)`.
fn cellmap_batch_keys(
    n: u64,
    domain: Option<Domain>,
    write: impl Fn(u64, u64) -> (u64, u64) + Send + 'static,
) -> Bench {
    let domain = domain.unwrap_or_else(Domain::root);
    let work = Arc::new(AtomicU64::new(0));
    let changes = Arc::new(AtomicU64::new(0));
    let (map, guard) = domain.enter(|| {
        let map = CellMap::<u64, u64>::new();
        for k in 0..n {
            map.insert(k, 0);
        }
        let (w, c) = (Arc::clone(&work), Arc::clone(&changes));
        let guard = map.subscribe_diffs(move |diff: &MapDiff<u64, u64>| {
            w.fetch_add(1, Ordering::Relaxed);
            let rows = match diff {
                MapDiff::Initial { .. } => 0,
                MapDiff::Batch { changes } => changes.len() as u64,
                _ => 1,
            };
            c.fetch_add(rows, Ordering::Relaxed);
        });
        (map, guard)
    });
    let ops_done = Arc::new(AtomicU64::new(0));
    let done = Arc::clone(&ops_done);
    let m = map.clone();
    Bench {
        run: Box::new(move |first, count| {
            for i in first..first + count {
                domain.batch(|| {
                    for k in 0..n {
                        let (key, value) = write(i, k);
                        m.insert(key, black_box(value));
                    }
                });
            }
            done.fetch_add(count, Ordering::Relaxed);
        }),
        work,
        errors: Box::new(move || {
            changes
                .load(Ordering::Relaxed)
                .abs_diff(ops_done.load(Ordering::Relaxed) * n)
        }),
        _keep: Box::new((map, guard)),
    }
}

/// `source -> derived -> consumer thread`. One batch writes `n` rows to
/// `source`; a diff subscriber copies each row into `derived` while the batch
/// is delivered, so every derived row is its own write and its own delivery;
/// `derived`'s diff subscriber hands each diff to a consumer thread, which
/// drops it. Work counts derived deliveries.
fn cellmap_derived_rows(n: u64) -> Bench {
    const CONSUMERS: usize = 8;
    fn rows<'a>(diff: &'a MapDiff<u64, u64>, out: &mut Vec<(&'a u64, &'a u64)>) {
        match diff {
            MapDiff::Insert { key, value } => out.push((key, value)),
            MapDiff::Update { key, new_value, .. } => out.push((key, new_value)),
            MapDiff::Batch { changes } => changes.iter().for_each(|d| rows(d, out)),
            MapDiff::Initial { .. } | MapDiff::Remove { .. } => {}
        }
    }
    let work = Arc::new(AtomicU64::new(0));
    let (senders, consumers): (Vec<_>, Vec<_>) = (0..CONSUMERS)
        .map(|_| {
            let (tx, rx) = std::sync::mpsc::channel::<Box<MapDiff<u64, u64>>>();
            let consumer = std::thread::spawn(move || {
                while let Ok(diff) = rx.recv() {
                    black_box(format!("{diff:?}"));
                }
            });
            (tx, consumer)
        })
        .unzip();
    let source = CellMap::<u64, u64>::new();
    let derived = CellMap::<u64, u64>::new();
    let d = derived.clone();
    let copy = source.subscribe_diffs(move |diff: &MapDiff<u64, u64>| {
        let mut out = Vec::new();
        rows(diff, &mut out);
        for (key, value) in out {
            d.insert(*key, *value);
        }
    });
    let (w, next) = (Arc::clone(&work), AtomicUsize::new(0));
    let send = derived.subscribe_diffs(move |diff: &MapDiff<u64, u64>| {
        if !matches!(diff, MapDiff::Initial { .. }) {
            w.fetch_add(1, Ordering::Relaxed);
            let lane = next.fetch_add(1, Ordering::Relaxed) % CONSUMERS;
            drop(senders[lane].send(Box::new(diff.clone())));
        }
    });
    let ops_done = Arc::new(AtomicU64::new(0));
    let (done, s) = (Arc::clone(&ops_done), source.clone());
    let delivered = Arc::clone(&work);
    Bench {
        run: Box::new(move |first, count| {
            for i in first..first + count {
                hyphae::batch(|| {
                    for k in 0..n {
                        s.insert(k, black_box(i + 1));
                    }
                });
            }
            done.fetch_add(count, Ordering::Relaxed);
        }),
        work,
        errors: Box::new(move || {
            delivered
                .load(Ordering::Relaxed)
                .abs_diff(ops_done.load(Ordering::Relaxed) * n)
        }),
        _keep: Box::new((source, derived, copy, send, consumers)),
    }
}

/// `outer -> switch_map(cached inner) [-> map(+1) x tail, materialized] ->
/// subscriber`. `changing` alternates the outer key between two cached inner
/// cells; otherwise every operation rewrites the outer with the value it
/// already holds. Work counts calls to the `switch_map` closure (re-knits).
fn switch_map(changing: bool, tail: usize) -> Bench {
    let work = Arc::new(AtomicU64::new(0));
    let delivered = Arc::new(AtomicU64::new(0));
    let inners = Arc::new([Cell::new(100u64), Cell::new(200u64)]);
    let outer = Cell::new(0u64);
    let (knits, i2) = (Arc::clone(&work), Arc::clone(&inners));
    let switched = outer
        .clone()
        .switch_map(move |key: &u64| {
            knits.fetch_add(1, Ordering::Relaxed);
            i2[(*key % 2) as usize].clone()
        })
        .materialize();
    let mut cells = vec![switched];
    for _ in 0..tail {
        let next = cells[cells.len() - 1]
            .clone()
            .map(|x| x.wrapping_add(1))
            .materialize();
        cells.push(next);
    }
    let (guard, last) = sink(&cells[cells.len() - 1], &delivered);
    let expected = Arc::new(AtomicU64::new(0));
    let exp = Arc::clone(&expected);
    let src = outer.clone();
    let t = tail as u64;
    Bench {
        run: Box::new(move |first, count| {
            let key = |i: u64| if changing { i % 2 } else { 0 };
            for i in first..first + count {
                batch(|| src.set(key(i)));
            }
            let inner = if key(first + count - 1) == 0 {
                100
            } else {
                200
            };
            exp.store(inner + t, Ordering::Relaxed);
        }),
        work,
        errors: expect_last(last, expected),
        _keep: Box::new((outer, inners, cells, guard, delivered)),
    }
}

/// `threads` writers, each doing `count / threads` batched writes. Disjoint:
/// each writer owns `source -> map -> materialize -> subscriber`. Overlapping:
/// each writer owns a source and all of them feed one `join_vec` sink.
fn writers(threads: usize, overlapping: bool) -> Bench {
    let work = Arc::new(AtomicU64::new(0));
    let sources: Vec<Cell<u64, CellMutable>> = (0..threads).map(|_| Cell::new(0)).collect();
    let mut keep: Vec<Box<dyn std::any::Any>> = Vec::new();
    let mut lasts = Vec::new();
    if overlapping {
        let joined = join_vec(sources.iter().cloned().map(Cell::lock).collect()).materialize();
        let summed = joined
            .clone()
            .map(|v: &Vec<u64>| v.iter().sum::<u64>())
            .materialize();
        let (guard, last) = sink(&summed, &work);
        lasts.push(last);
        keep.push(Box::new((joined, summed, guard)));
    } else {
        for source in &sources {
            let cell = source.clone().map(|x| x.wrapping_add(1)).materialize();
            let (guard, last) = sink(&cell, &work);
            lasts.push(last);
            keep.push(Box::new((cell, guard)));
        }
    }
    let srcs = sources.clone();
    let finals: Arc<Vec<AtomicU64>> = Arc::new((0..threads).map(|_| AtomicU64::new(0)).collect());
    let f2 = Arc::clone(&finals);
    Bench {
        run: Box::new(move |first, count| {
            let per = count / threads as u64;
            let barrier = Barrier::new(threads);
            thread::scope(|scope| {
                for (t, source) in srcs.iter().enumerate() {
                    let (barrier, finals) = (&barrier, &f2);
                    scope.spawn(move || {
                        barrier.wait();
                        for i in first..first + per {
                            batch(|| source.set(black_box(i)));
                        }
                        finals[t].store(first + per - 1, Ordering::Relaxed);
                    });
                }
            });
        }),
        work,
        // Concurrent batches coalesce, so a sink sees only some writes; it
        // must still settle on the last one.
        errors: Box::new(move || {
            let finals: Vec<u64> = finals.iter().map(|f| f.load(Ordering::Relaxed)).collect();
            if overlapping {
                u64::from(lasts[0].load(Ordering::Relaxed) != finals.iter().sum::<u64>())
            } else {
                lasts
                    .iter()
                    .zip(&finals)
                    .filter(|(l, f)| l.load(Ordering::Relaxed) != *f + 1)
                    .count() as u64
            }
        }),
        _keep: Box::new((sources, keep)),
    }
}

/// Reference: the same one-stage graph written outside any batch, which
/// bypasses the scheduler queue.
fn eager_map() -> Bench {
    let work = Arc::new(AtomicU64::new(0));
    let source = Cell::new(0u64);
    let cell = source.clone().map(|x| x.wrapping_add(1)).materialize();
    let (guard, last) = sink(&cell, &work);
    let expected = Arc::new(AtomicU64::new(0));
    let exp = Arc::clone(&expected);
    let src = source.clone();
    Bench {
        run: Box::new(move |first, count| {
            for i in first..first + count {
                src.set(black_box(i));
            }
            exp.store(first + count, Ordering::Relaxed);
        }),
        work,
        errors: expect_last(last, expected),
        _keep: Box::new((source, cell, guard)),
    }
}

/// `source -> map -> filter -> tap -> try_map -> map -> subscriber`, fused into
/// one cell, one batched write per operation.
fn fused_mixed() -> Bench {
    let work = Arc::new(AtomicU64::new(0));
    let source = Cell::new(0u64);
    let cell = source
        .clone()
        .map(|x| x.wrapping_add(1))
        .filter(|x| *x != u64::MAX)
        .tap(|x| {
            black_box(x);
        })
        .try_map(|x| Ok::<u64, u64>(x.wrapping_add(1)))
        .map(|r| r.map_or(0, |x| x.wrapping_add(1)))
        .materialize();
    let last = Arc::new(AtomicU64::new(u64::MAX));
    let (w, l) = (Arc::clone(&work), Arc::clone(&last));
    let guard = cell.subscribe(move |signal| {
        if let Signal::Value(value) = signal
            && let Some(value) = **value
        {
            w.fetch_add(1, Ordering::Relaxed);
            l.store(value, Ordering::Relaxed);
        }
    });
    let expected = Arc::new(AtomicU64::new(0));
    let exp = Arc::clone(&expected);
    let src = source.clone();
    Bench {
        run: Box::new(move |first, count| {
            for i in first..first + count {
                batch(|| src.set(black_box(i)));
            }
            exp.store((first + count - 1).wrapping_add(3), Ordering::Relaxed);
        }),
        work,
        errors: expect_last(last, expected),
        _keep: Box::new((source, cell, guard)),
    }
}

fn cases() -> Vec<Case> {
    let mut cases = vec![
        case("eager_map_one", "reference", 200_000, eager_map),
        case("batch_map_one", "H1 H2 H3 H7", 200_000, || {
            cell_chain(1, None)
        }),
        case("fused_chain/4", "H5", 200_000, || fused_chain!(4, 3)),
        case("fused_chain/8", "H5", 200_000, || fused_chain!(8, 7)),
        case("fused_mixed", "H5", 200_000, fused_mixed),
        case("cell_chain/4", "H1 H2 H3 H7", 50_000, || {
            cell_chain(4, None)
        }),
        case("cell_chain/16", "H1 H2 H3 H7", 20_000, || {
            cell_chain(16, None)
        }),
        case("fan_out/16", "H1 H2 H3", 20_000, || fan_out(16)),
        case("fan_out/128", "H1 H2 H3", 4_000, || fan_out(128)),
        case("switch_map/changed", "H6", 50_000, || switch_map(true, 0)),
        case("switch_map/equal", "H6", 50_000, || switch_map(false, 0)),
        case("graph_edit/4", "H7", 20_000, || switch_map(true, 4)),
        case("graph_edit/16", "H7", 10_000, || switch_map(true, 16)),
    ];
    for (n, ops) in [(1, 100_000), (16, 20_000), (256, 1_000)] {
        cases.push(case(format!("cellmap_batch/{n}"), "H4", ops, move || {
            cellmap_batch(n, None)
        }));
    }
    cases.push(case("cellmap_batch_same_key/16", "H4", 20_000, || {
        cellmap_batch_keys(16, None, |i, k| (0, i * 16 + k + 1))
    }));
    cases.push(case("cellmap_batch_bulk/50000", "H4", 20, || {
        cellmap_batch(50_000, None)
    }));
    for (n, ops) in [(1_000, 200), (10_000, 20)] {
        cases.push(case(
            format!("cellmap_derived_rows/{n}"),
            "H4",
            ops,
            move || cellmap_derived_rows(n),
        ));
    }
    for threads in [1usize, 4] {
        cases.push(case(
            format!("writers/disjoint/{threads}"),
            "H2",
            200_000,
            move || writers(threads, false),
        ));
        cases.push(case(
            format!("writers/overlapping/{threads}"),
            "H2",
            100_000,
            move || writers(threads, true),
        ));
    }
    cases.push(case("domain/batch_map_one", "H1 H2 H3 H7", 200_000, || {
        cell_chain(1, Some(Domain::child()))
    }));
    cases.push(case("domain/cell_chain/4", "H1 H2 H3 H7", 50_000, || {
        cell_chain(4, Some(Domain::child()))
    }));
    cases.push(case("domain/cellmap_batch/16", "H4", 20_000, || {
        cellmap_batch(16, Some(Domain::child()))
    }));
    cases
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--bench")
        .collect();
    let list = args.iter().any(|a| a == "--list");
    let filters: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    for case in cases() {
        if !filters.is_empty() && !filters.iter().any(|f| case.name.contains(f.as_str())) {
            continue;
        }
        if list {
            println!("{}", case.name);
            continue;
        }
        let mut bench = (case.build)();
        (bench.run)(0, WARMUP_OPS);
        let mut next = WARMUP_OPS;
        let work_before = bench.work.load(Ordering::Relaxed);
        let (allocs_before, bytes_before) = (
            ALLOC_CALLS.load(Ordering::Relaxed),
            ALLOC_BYTES.load(Ordering::Relaxed),
        );
        let mut ns = Vec::with_capacity(REPEATS);
        for _ in 0..REPEATS {
            let start = Instant::now();
            (bench.run)(next, case.ops);
            ns.push(start.elapsed().as_nanos() as f64 / case.ops as f64);
            next += case.ops;
        }
        let allocs = ALLOC_CALLS.load(Ordering::Relaxed) - allocs_before;
        let bytes = ALLOC_BYTES.load(Ordering::Relaxed) - bytes_before;
        let total = (case.ops * REPEATS as u64) as f64;
        let work = bench.work.load(Ordering::Relaxed) - work_before;
        let errors = (bench.errors)();
        let ns_list = ns
            .iter()
            .map(|v| format!("{v:.2}"))
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{{\"case\":\"{}\",\"covers\":\"{}\",\"ops\":{},\"repeats\":{REPEATS},\
             \"allocs_per_op\":{:.4},\"bytes_per_op\":{:.2},\"work_per_op\":{:.4},\
             \"errors\":{errors},\"ns_per_op\":[{ns_list}]}}",
            case.name,
            case.covers,
            case.ops,
            allocs as f64 / total,
            bytes as f64 / total,
            work as f64 / total,
        );
    }
}
