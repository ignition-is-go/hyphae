//! Lock-hold costs of `CellMap::register_write_hook`.
//!
//! `cargo run --release --example cell_map_write_hook -- eq` measures the
//! equality check a registered hook moves under the shard write lock, for a
//! small and a ~100 KB value whose `PartialEq` is a deep compare, and the
//! latency a reader of another key in the same shard sees meanwhile.
//!
//! `... -- seed` measures how long registration on a hydrated map blocks a
//! concurrent writer.

use std::{
    hint::black_box,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use hyphae::{CellMap, SubscriptionGuard, cell_map::MapWrite};

/// A row whose equality is a deep comparison, like an entity behind
/// `Arc<dyn Trait>` (no pointer-equality shortcut).
#[derive(Clone, Debug)]
struct Row(Arc<Vec<u8>>);

impl PartialEq for Row {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_slice() == other.0.as_slice()
    }
}

fn index_hook(map: &CellMap<u64, Row>) -> SubscriptionGuard {
    let index = dashmap::DashMap::<u64, usize>::new();
    map.register_write_hook(move |write| match write {
        MapWrite::Insert { key, value }
        | MapWrite::Update {
            key, new: value, ..
        } => {
            index.insert(*key, value.0.len());
        }
        MapWrite::Remove { key, .. } => {
            index.remove(key);
        }
    })
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p) as usize]
}

fn eq() {
    println!("size\thook\twrite ns\treader p50 ns\treader p99 ns\treader max us");
    for size in [64usize, 100_000] {
        for hooked in [false, true] {
            let map = CellMap::<u64, Row>::new();
            for key in 0..64 {
                map.insert(key, Row(Arc::new(vec![0; size])));
            }
            let _hook = hooked.then(|| index_hook(&map));
            // Two distinct buffers that differ only in the last byte, so the
            // comparison reads the whole value every time.
            let a = Row(Arc::new(vec![0; size]));
            let mut b_bytes = vec![0; size];
            b_bytes[size - 1] = 1;
            let b = Row(Arc::new(b_bytes));
            let stop = AtomicBool::new(false);
            let writes = 20_000;
            let (write_ns, mut lat) = std::thread::scope(|scope| {
                let reader = scope.spawn(|| {
                    let mut lat = Vec::with_capacity(1 << 20);
                    let mut key = 1;
                    while !stop.load(Ordering::Relaxed) {
                        key = key % 63 + 1;
                        let start = Instant::now();
                        black_box(map.get_value(&key));
                        lat.push(start.elapsed());
                    }
                    lat
                });
                let start = Instant::now();
                for i in 0..writes {
                    let row = if i % 2 == 0 { a.clone() } else { b.clone() };
                    drop(black_box(map.insert(0, row)));
                }
                let ns = start.elapsed().as_nanos() as f64 / writes as f64;
                stop.store(true, Ordering::Relaxed);
                (ns, reader.join().unwrap_or_default())
            });
            lat.sort_unstable();
            println!(
                "{size}\t{hooked}\t{write_ns:.0}\t{}\t{}\t{:.1}",
                percentile(&lat, 0.5).as_nanos(),
                percentile(&lat, 0.99).as_nanos(),
                lat.last().map_or(0.0, |d| d.as_secs_f64() * 1e6),
            );
        }
    }
}

fn seed() {
    println!("rows\tregister ms\twriter max stall ms");
    for rows in [100_000u64, 500_000] {
        let map = CellMap::<u64, Row>::new();
        let row = Row(Arc::new(vec![0; 64]));
        map.insert_many((0..rows).map(|k| (k, row.clone())).collect());
        let stop = AtomicBool::new(false);
        let (register, stall) = std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                let mut worst = Duration::ZERO;
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let start = Instant::now();
                    map.insert(rows + i % 1024, row.clone());
                    worst = worst.max(start.elapsed());
                    i += 1;
                }
                worst
            });
            std::thread::sleep(Duration::from_millis(20));
            let start = Instant::now();
            let guard = index_hook(&map);
            let register = start.elapsed();
            std::thread::sleep(Duration::from_millis(20));
            stop.store(true, Ordering::Relaxed);
            drop(guard);
            (register, writer.join().unwrap_or_default())
        });
        println!(
            "{rows}\t{:.1}\t{:.1}",
            register.as_secs_f64() * 1e3,
            stall.as_secs_f64() * 1e3
        );
    }
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("seed") => seed(),
        _ => eq(),
    }
}
