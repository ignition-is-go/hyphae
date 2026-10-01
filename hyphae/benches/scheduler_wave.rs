use std::{
    alloc::{GlobalAlloc, Layout, System},
    env,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use hyphae::{
    Cell, CellImmutable, CellMutable, FilterExt, MapExt, Materialize, Mutable, Signal,
    SubscriptionGuard, Watchable, batch,
};
use parking_lot::Mutex;

const WIDE: (usize, usize) = (256, 16);
const NARROW: (usize, usize) = (4, 2);
const WARMUP: u64 = 100;
const SAMPLES: u64 = 1000;
const CLOCK_TICKS: u64 = 480;
const CLOCK_COMMANDS: u64 = 120;
const CLOCK_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / 240);
const COMMAND_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / 60);

struct CountingAllocator;
static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(
            u64::try_from(layout.size()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(
            u64::try_from(layout.size()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(u64::try_from(size).unwrap_or(u64::MAX), Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }
}

struct Wave {
    source: Cell<u64, CellMutable>,
    notifications: Arc<AtomicU64>,
    last: Arc<AtomicU64>,
    _guards: Vec<SubscriptionGuard>,
}

impl Wave {
    fn new(width: usize, fanout: usize) -> Self {
        let source = Cell::new(0_u64);
        let notifications = Arc::new(AtomicU64::new(0));
        let last = Arc::new(AtomicU64::new(0));
        let mut guards = Vec::with_capacity(width.saturating_mul(fanout));
        for branch in 0..width {
            let branch = u64::try_from(branch).unwrap_or(u64::MAX);
            let middle = source
                .clone()
                .map(move |v| v.saturating_add(branch))
                .materialize();
            for leaf in 0..fanout {
                let leaf = u64::try_from(leaf).unwrap_or(u64::MAX);
                let count = notifications.clone();
                let settled = last.clone();
                let output = middle
                    .clone()
                    .map(move |v| v.saturating_add(leaf))
                    .materialize();
                guards.push(output.subscribe(move |signal| {
                    if let Signal::Value(value) = signal {
                        count.fetch_add(1, Ordering::Relaxed);
                        settled.fetch_max(**value, Ordering::Relaxed);
                    }
                }));
            }
        }
        Self {
            source,
            notifications,
            last,
            _guards: guards,
        }
    }

    fn set(&self, value: u64) {
        batch(|| self.source.set(value));
    }

    fn reset(&self) {
        self.notifications.store(0, Ordering::Relaxed);
        self.last.store(0, Ordering::Relaxed);
    }
}

fn snapshot() -> (u64, u64) {
    (
        ALLOC_CALLS.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
    )
}

fn delta(before: (u64, u64)) -> (u64, u64) {
    let after = snapshot();
    (
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
    )
}

fn percentile(sorted: &[u128], p: usize) -> u128 {
    sorted
        .get(sorted.len().saturating_sub(1).saturating_mul(p) / 100)
        .copied()
        .unwrap_or_default()
}

fn gaps(times: &[Instant]) -> Vec<u128> {
    let mut result: Vec<_> = times
        .windows(2)
        .filter_map(|window| match window {
            [first, second] => second
                .checked_duration_since(*first)
                .map(|gap| gap.as_nanos()),
            _ => None,
        })
        .collect();
    result.sort_unstable();
    result
}

fn run_wave(mode: &str, width: usize, fanout: usize, samples: u64) {
    let wave = Wave::new(width, fanout);
    for value in 1..=WARMUP {
        wave.set(value);
    }
    wave.reset();
    let mut durations = Vec::with_capacity(usize::try_from(samples).unwrap_or(usize::MAX));
    let allocations = snapshot();
    let start = Instant::now();
    for value in WARMUP.saturating_add(1)..=WARMUP.saturating_add(samples) {
        let tick = Instant::now();
        wave.set(value);
        durations.push(tick.elapsed().as_nanos());
    }
    let elapsed = start.elapsed();
    let allocations = delta(allocations);
    let expected =
        samples.saturating_mul(u64::try_from(width.saturating_mul(fanout)).unwrap_or(u64::MAX));
    assert_eq!(wave.notifications.load(Ordering::Relaxed), expected);
    assert_eq!(
        wave.last.load(Ordering::Relaxed),
        WARMUP
            .saturating_add(samples)
            .saturating_add(u64::try_from(width.saturating_sub(1)).unwrap_or(u64::MAX))
            .saturating_add(u64::try_from(fanout.saturating_sub(1)).unwrap_or(u64::MAX))
    );
    durations.sort_unstable();
    println!(
        "{{\"mode\":\"{mode}\",\"width\":{width},\"fanout\":{fanout},\"samples\":{samples},\"duration_ns\":{},\"p50_ns\":{},\"p95_ns\":{},\"p99_ns\":{},\"max_ns\":{},\"notifications\":{expected},\"alloc_calls\":{},\"alloc_bytes\":{}}}",
        elapsed.as_nanos(),
        percentile(&durations, 50),
        percentile(&durations, 95),
        percentile(&durations, 99),
        durations.last().copied().unwrap_or_default(),
        allocations.0,
        allocations.1
    );
}

fn sleep_until(deadline: Instant) {
    if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        thread::sleep(remaining);
    }
}

fn deadline(start: Instant, period: Duration, index: u64) -> Instant {
    let offset = period.saturating_mul(u32::try_from(index).unwrap_or(u32::MAX));
    start.checked_add(offset).unwrap_or(start)
}

fn verify_clock_output(
    count60: u64,
    count30: u64,
    last60: u64,
    last30: u64,
    expected60: u64,
    expected30: u64,
    ticks: u64,
) {
    assert!((1..=expected60).contains(&count60));
    assert!((1..=expected30).contains(&count30));
    assert_eq!(last60, ticks);
    assert_eq!(last30, ticks);
}

#[allow(clippy::too_many_lines)]
fn run_clock(loaded: bool, periods: u64) {
    let ticks = CLOCK_TICKS.saturating_mul(periods);
    let commands = CLOCK_COMMANDS.saturating_mul(periods);
    let source = Cell::new(0_u64);
    let hz60 = source.clone().filter(|tick| tick % 4 == 0).materialize();
    let hz30 = source.clone().filter(|tick| tick % 8 == 0).materialize();
    let times60 = Arc::new(Mutex::new(Vec::with_capacity(120)));
    let times30 = Arc::new(Mutex::new(Vec::with_capacity(60)));
    let count60 = Arc::new(AtomicU64::new(0));
    let count30 = Arc::new(AtomicU64::new(0));
    let last60 = Arc::new(AtomicU64::new(0));
    let last30 = Arc::new(AtomicU64::new(0));
    let guard60 = subscribe_ticks(&hz60, &times60, &count60, &last60);
    let guard30 = subscribe_ticks(&hz30, &times30, &count30, &last30);
    source.set(1);
    count60.store(0, Ordering::Relaxed);
    count30.store(0, Ordering::Relaxed);
    times60.lock().clear();
    times30.lock().clear();

    let heavy = loaded.then(|| Arc::new(Wave::new(WIDE.0, WIDE.1)));
    if let Some(wave) = &heavy {
        wave.set(1);
        wave.reset();
    }
    let drains = Arc::new(Mutex::new(Vec::with_capacity(
        usize::try_from(commands).unwrap_or(usize::MAX),
    )));
    let allocations = snapshot();
    let start = Instant::now();
    thread::scope(|scope| {
        let source = source.clone();
        scope.spawn(move || {
            for tick in 1..=ticks {
                sleep_until(deadline(start, CLOCK_PERIOD, tick));
                batch(|| source.set(tick));
            }
        });
        if let Some(wave) = &heavy {
            let drains = drains.clone();
            scope.spawn(move || {
                for command in 1..=commands {
                    sleep_until(deadline(start, COMMAND_PERIOD, command));
                    let began = Instant::now();
                    wave.set(command.saturating_add(1));
                    drains.lock().push(began.elapsed().as_nanos());
                }
            });
        }
    });
    let elapsed = start.elapsed();
    let tail = elapsed
        .saturating_sub(CLOCK_PERIOD.saturating_mul(u32::try_from(ticks).unwrap_or(u32::MAX)));
    let allocations = delta(allocations);
    let expected60 = ticks / 4;
    let expected30 = ticks / 8;
    verify_clock_output(
        count60.load(Ordering::Relaxed),
        count30.load(Ordering::Relaxed),
        last60.load(Ordering::Relaxed),
        last30.load(Ordering::Relaxed),
        expected60,
        expected30,
        ticks,
    );
    let expected_notifications = if loaded {
        commands.saturating_mul(u64::try_from(WIDE.0.saturating_mul(WIDE.1)).unwrap_or(u64::MAX))
    } else {
        0
    };
    let notifications = heavy
        .as_ref()
        .map_or(0, |wave| wave.notifications.load(Ordering::Relaxed));
    if let Some(wave) = &heavy {
        assert!((1..=expected_notifications).contains(&notifications));
        assert_eq!(
            wave.last.load(Ordering::Relaxed),
            commands
                .saturating_add(1)
                .saturating_add(u64::try_from(WIDE.0.saturating_sub(1)).unwrap_or(u64::MAX))
                .saturating_add(u64::try_from(WIDE.1.saturating_sub(1)).unwrap_or(u64::MAX))
        );
    }
    let gap60 = gaps(&times60.lock());
    let gap30 = gaps(&times30.lock());
    let mut drains = drains.lock();
    drains.sort_unstable();
    let mode = match (loaded, periods) {
        (true, 1) => "clock",
        (true, _) => "clock-long",
        (false, _) => "idle",
    };
    println!(
        "{{\"mode\":\"{mode}\",\"duration_ns\":{},\"tail_ns\":{},\"source_ticks\":{ticks},\"hz60_expected\":{expected60},\"hz60_samples\":{},\"hz60_gap_p50_ns\":{},\"hz60_gap_p95_ns\":{},\"hz60_gap_p99_ns\":{},\"hz60_gap_max_ns\":{},\"hz30_expected\":{expected30},\"hz30_samples\":{},\"hz30_gap_p50_ns\":{},\"hz30_gap_p95_ns\":{},\"hz30_gap_p99_ns\":{},\"hz30_gap_max_ns\":{},\"command_samples\":{},\"notifications_expected\":{expected_notifications},\"notifications\":{notifications},\"drain_p50_ns\":{},\"drain_p95_ns\":{},\"drain_p99_ns\":{},\"drain_max_ns\":{},\"alloc_calls\":{},\"alloc_bytes\":{}}}",
        elapsed.as_nanos(),
        tail.as_nanos(),
        count60.load(Ordering::Relaxed),
        percentile(&gap60, 50),
        percentile(&gap60, 95),
        percentile(&gap60, 99),
        gap60.last().copied().unwrap_or_default(),
        count30.load(Ordering::Relaxed),
        percentile(&gap30, 50),
        percentile(&gap30, 95),
        percentile(&gap30, 99),
        gap30.last().copied().unwrap_or_default(),
        drains.len(),
        percentile(&drains, 50),
        percentile(&drains, 95),
        percentile(&drains, 99),
        drains.last().copied().unwrap_or_default(),
        allocations.0,
        allocations.1
    );
    drop(drains);
    drop((guard60, guard30));
}

fn subscribe_ticks(
    cell: &Cell<Option<u64>, CellImmutable>,
    times: &Arc<Mutex<Vec<Instant>>>,
    count: &Arc<AtomicU64>,
    last: &Arc<AtomicU64>,
) -> SubscriptionGuard {
    let times = times.clone();
    let count = count.clone();
    let last = last.clone();
    cell.subscribe(move |signal| {
        if let Signal::Value(value) = signal
            && let Some(tick) = value.as_ref()
        {
            count.fetch_add(1, Ordering::Relaxed);
            last.store(*tick, Ordering::Relaxed);
            times.lock().push(Instant::now());
        }
    })
}

fn main() {
    match env::args().nth(1).as_deref() {
        None | Some("wide") => run_wave("wide", WIDE.0, WIDE.1, SAMPLES),
        Some("narrow") => run_wave("narrow", NARROW.0, NARROW.1, 100_000),
        Some("clock") => run_clock(true, 1),
        Some("clock-long") => run_clock(true, 5),
        Some("idle") => run_clock(false, 1),
        Some(mode) => {
            eprintln!("unknown mode {mode:?}; expected wide, narrow, clock, clock-long, or idle");
            std::process::exit(2);
        }
    }
}
