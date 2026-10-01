//! Run with `taskset -c <32-CPU-set> cargo run -p hyphae --release --example
//! owned_guard_allocations -- cell 10000 0`; repeat with one guard and `source`.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use hyphae::{Cell, Source, SubscriptionGuard};

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

struct Snapshot {
    calls: usize,
    allocated: usize,
    live: usize,
}

impl Snapshot {
    fn read() -> Self {
        Self {
            calls: ALLOC_CALLS.load(Ordering::Relaxed),
            allocated: ALLOC_BYTES.load(Ordering::Relaxed),
            live: LIVE_BYTES.load(Ordering::Relaxed),
        }
    }
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let kind = arguments.next().unwrap_or_else(|| "cell".to_owned());
    let count = arguments
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_000usize);
    let guards = arguments
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0usize);
    if kind != "cell" && kind != "source" {
        eprintln!("usage: owned_guard_allocations [cell|source] [count] [guards_per_owner]");
        std::process::exit(2);
    }

    let warmup = Cell::new(0u64);
    warmup.own(SubscriptionGuard::from_callback(|| {}));
    drop(warmup);
    drop(Source::<u64>::new());
    let cleanups = Arc::new(AtomicUsize::new(0));
    let mut cells = Vec::with_capacity(if kind == "cell" { count } else { 0 });
    let mut sources = Vec::with_capacity(if kind == "source" { count } else { 0 });
    let available_cpus = std::thread::available_parallelism().map_or(0, usize::from);
    let before = Snapshot::read();
    let started = Instant::now();
    for index in 0..count {
        if kind == "cell" {
            let cell = Cell::new(u64::try_from(index).unwrap_or(u64::MAX));
            for _ in 0..guards {
                let cleanups = cleanups.clone();
                cell.own(SubscriptionGuard::from_callback(move || {
                    cleanups.fetch_add(1, Ordering::Relaxed);
                }));
            }
            cells.push(cell);
        } else {
            let source = Source::<u64>::new();
            for _ in 0..guards {
                let cleanups = cleanups.clone();
                source.own(SubscriptionGuard::from_callback(move || {
                    cleanups.fetch_add(1, Ordering::Relaxed);
                }));
            }
            sources.push(source);
        }
    }
    let construction_ns = started.elapsed().as_nanos();
    black_box((&cells, &sources));
    let retained = Snapshot::read();
    let drop_started = Instant::now();
    cells.clear();
    sources.clear();
    let drop_ns = drop_started.elapsed().as_nanos();
    let released = Snapshot::read();
    let dropped_guards = cleanups.load(Ordering::Relaxed);
    assert_eq!(Some(dropped_guards), count.checked_mul(guards));
    assert_eq!(released.live, before.live);
    println!(
        "{{\"kind\":\"{kind}\",\"count\":{count},\"guards\":{guards},\"available_cpus\":{available_cpus},\"alloc_calls\":{},\"alloc_bytes\":{},\"retained_bytes\":{},\"remaining_bytes\":{},\"construction_ns\":{construction_ns},\"drop_ns\":{drop_ns},\"dropped_guards\":{dropped_guards}}}",
        retained.calls.saturating_sub(before.calls),
        retained.allocated.saturating_sub(before.allocated),
        retained.live.saturating_sub(before.live),
        released.live.saturating_sub(before.live),
    );
}
