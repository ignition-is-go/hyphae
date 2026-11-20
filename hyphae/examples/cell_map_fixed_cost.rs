//! Per-map fixed cost of `CellMap`: bytes to create an empty map, and
//! allocations and time for `snapshot`, `entries`, `for_each` and
//! `subscribe_diffs` by map size; then write throughput with distinct keys per
//! thread on one large map and on many small maps.
//!
//! Run with `cargo run -p hyphae --release --example cell_map_fixed_cost --
//! costs` and `-- contention`. Allocation counts do not depend on load; times
//! are only comparable between runs taken back to back.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};

use hyphae::{CellMap, Gettable, MapDiff, Materialize};

struct CountingAllocator;

static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let resized = unsafe { System.realloc(pointer, layout, new_size) };
        if !resized.is_null() {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(new_size, Ordering::Relaxed);
            LIVE_BYTES.fetch_add(new_size, Ordering::Relaxed);
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        resized
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Clone, Copy)]
struct Counts {
    calls: usize,
    bytes: usize,
    live: usize,
}

fn counts() -> Counts {
    Counts {
        calls: ALLOC_CALLS.load(Ordering::Relaxed),
        bytes: ALLOC_BYTES.load(Ordering::Relaxed),
        live: LIVE_BYTES.load(Ordering::Relaxed),
    }
}

fn filled(rows: u64) -> CellMap<u64, u64> {
    let map = CellMap::new();
    map.insert_many((0..rows).map(|k| (k, k)).collect());
    map
}

/// Allocation calls and bytes per call of `op` (exact, averaged over `reps`)
/// and ns per call.
fn measure(reps: usize, mut op: impl FnMut()) -> (f64, f64, f64) {
    op();
    let before = counts();
    let start = Instant::now();
    for _ in 0..reps {
        op();
    }
    let elapsed = start.elapsed();
    let after = counts();
    (
        (after.calls - before.calls) as f64 / reps as f64,
        (after.bytes - before.bytes) as f64 / reps as f64,
        elapsed.as_nanos() as f64 / reps as f64,
    )
}

fn run_op(map: &CellMap<u64, u64>, name: &str) {
    match name {
        "snapshot" => drop(black_box(map.snapshot())),
        "for_each" => {
            let mut sum = 0u64;
            map.for_each(|_, v| sum = sum.wrapping_add(*v));
            black_box(sum);
        }
        "entries" => drop(black_box(map.entries().materialize().get())),
        "subscribe_diffs" => drop(black_box(map.subscribe_diffs(|diff: &MapDiff<u64, u64>| {
            black_box(diff);
        }))),
        "insert" => drop(black_box(map.insert(u64::MAX, 1))),
        "get_value" => drop(black_box(map.get_value(&0))),
        _ => {}
    }
}

/// `op <name> <rows> <reps>`: run one operation `reps` times after setup, for
/// `perf stat` runs whose difference against `reps = 0` excludes the setup.
fn op_only(args: &[String]) {
    let name = args.first().map_or("snapshot", String::as_str);
    let parse = |i: usize| args.get(i).and_then(|a| a.parse::<u64>().ok()).unwrap_or(0);
    let map = filled(parse(1));
    for _ in 0..parse(2) {
        run_op(&map, name);
    }
}

fn costs() {
    let parallelism = std::thread::available_parallelism().map_or(1, usize::from);
    println!("available_parallelism={parallelism}");

    const MAPS: usize = 1000;
    let before = counts();
    let maps: Vec<CellMap<u64, u64>> = (0..MAPS).map(|_| CellMap::new()).collect();
    let after = counts();
    println!(
        "empty CellMap: {:.1} allocs, {:.0} bytes allocated, {:.0} bytes retained per map",
        (after.calls - before.calls) as f64 / MAPS as f64,
        (after.bytes - before.bytes) as f64 / MAPS as f64,
        (after.live - before.live) as f64 / MAPS as f64,
    );
    drop(maps);

    println!("rows\top\tallocs/call\tbytes/call\tns/call");
    for rows in [0u64, 1, 10, 1_000, 100_000] {
        let map = filled(rows);
        let reps = if rows >= 100_000 { 20 } else { 2_000 };
        let report = |name: &str, (allocs, bytes, ns): (f64, f64, f64)| {
            println!("{rows}\t{name}\t{allocs:.1}\t{bytes:.0}\t{ns:.0}");
        };
        report("snapshot", measure(reps, || drop(black_box(map.snapshot()))));
        report(
            "for_each",
            measure(reps, || {
                let mut sum = 0u64;
                map.for_each(|_, v| sum = sum.wrapping_add(*v));
                black_box(sum);
            }),
        );
        report("entries", measure(reps, || drop(black_box(map.entries().materialize().get()))));
        report(
            "subscribe_diffs",
            measure(reps, || {
                let guard = map.subscribe_diffs(|diff: &MapDiff<u64, u64>| {
                    black_box(diff);
                });
                drop(black_box(guard));
            }),
        );
    }
}

fn run_threads(threads: usize, work: impl Fn(usize) + Sync) -> f64 {
    let barrier = Barrier::new(threads);
    let start = Instant::now();
    std::thread::scope(|scope| {
        for t in 0..threads {
            let (barrier, work) = (&barrier, &work);
            scope.spawn(move || {
                barrier.wait();
                work(t);
            });
        }
    });
    start.elapsed().as_secs_f64()
}

fn contention() {
    const WRITES_PER_THREAD: u64 = 200_000;
    const LARGE_ROWS: u64 = 100_000;
    const SMALL_MAPS: usize = 4_096;
    let only: Option<usize> = std::env::args().nth(2).and_then(|a| a.parse().ok());
    println!("threads\tlarge Mwrites/s\treader get_value ns\tsmall Mwrites/s");
    for threads in [1usize, 4, 8, 24] {
        if only.is_some_and(|only| only != threads) {
            continue;
        }
        let large = filled(LARGE_ROWS);
        let stop = AtomicBool::new(false);
        let mut reader_ns = 0.0;
        let mut large_secs = 0.0;
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let start = Instant::now();
                let mut reads = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    black_box(large.get_value(&(reads * 7919 % LARGE_ROWS)));
                    reads += 1;
                }
                start.elapsed().as_nanos() as f64 / reads.max(1) as f64
            });
            large_secs = run_threads(threads, |t| {
                let base = t as u64 * (LARGE_ROWS / 24);
                for i in 0..WRITES_PER_THREAD {
                    large.insert(base + i % (LARGE_ROWS / 24), i);
                }
            });
            stop.store(true, Ordering::Relaxed);
            reader_ns = reader.join().unwrap_or(0.0);
        });
        let small: Arc<Vec<CellMap<u64, u64>>> =
            Arc::new((0..SMALL_MAPS).map(|_| filled(8)).collect());
        let small_secs = run_threads(threads, |t| {
            for i in 0..WRITES_PER_THREAD {
                let map = &small[(i as usize * 7 + t) % SMALL_MAPS];
                map.insert(100 + t as u64, i);
            }
        });
        let total = (threads as u64 * WRITES_PER_THREAD) as f64 / 1e6;
        println!(
            "{threads}\t{:.2}\t{reader_ns:.0}\t{:.2}",
            total / large_secs,
            total / small_secs
        );
    }
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("contention") => contention(),
        Some("op") => op_only(&std::env::args().skip(2).collect::<Vec<_>>()),
        _ => costs(),
    }
}
