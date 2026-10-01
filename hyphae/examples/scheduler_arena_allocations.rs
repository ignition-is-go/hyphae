//! Measure total allocation for isolated cells, chains, and fanout graphs.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use hyphae::{Cell, Watchable};

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
    let kind = arguments.next().unwrap_or_else(|| "isolated".to_owned());
    let count = arguments
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(256usize);
    assert!(matches!(kind.as_str(), "isolated" | "chain" | "fanout"));
    let warmup = Cell::new(0u64);
    let child = Cell::new(0u64);
    child.own(warmup.subscribe(|_| {}));
    drop(child);
    drop(warmup);
    let mut cells = Vec::with_capacity(count);
    let before = Snapshot::read();
    let started = Instant::now();
    for index in 0..count {
        let cell = Cell::new(u64::try_from(index).unwrap_or(u64::MAX));
        let source: Option<&Cell<u64, hyphae::CellMutable>> = match kind.as_str() {
            "chain" => cells.last(),
            "fanout" => cells.first(),
            _ => None,
        };
        if let Some(source) = source {
            cell.own(source.subscribe(|_| {}));
        }
        cells.push(cell);
    }
    let construction_ns = started.elapsed().as_nanos();
    black_box(&cells);
    let retained = Snapshot::read();
    let drop_started = Instant::now();
    while let Some(cell) = cells.pop() {
        drop(cell);
    }
    let drop_ns = drop_started.elapsed().as_nanos();
    let released = Snapshot::read();
    assert_eq!(released.live, before.live);
    println!(
        "{{\"kind\":\"{kind}\",\"count\":{count},\"alloc_calls\":{},\"alloc_bytes\":{},\"retained_bytes\":{},\"remaining_bytes\":{},\"construction_ns\":{construction_ns},\"drop_ns\":{drop_ns}}}",
        retained.calls.saturating_sub(before.calls),
        retained.allocated.saturating_sub(before.allocated),
        retained.live.saturating_sub(before.live),
        released.live.saturating_sub(before.live),
    );
}
