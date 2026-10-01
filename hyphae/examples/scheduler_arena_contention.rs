//! Count and verify every update in two concurrent, ordered pipelines.

#[cfg(feature = "scheduler")]
mod probe {
    use std::{
        env,
        hint::black_box,
        sync::{
            Arc,
            atomic::{AtomicI64, AtomicU64, Ordering},
        },
        thread,
        time::Instant,
    };

    use hyphae::{
        Cell, CellMutable, Gettable, MapExt, Materialize, Mutable, Signal, Watchable, batch,
        scheduler::no_coalesce,
    };

    const THREADS: usize = 2;
    const DEFAULT_OPS: u64 = 20_000;

    pub fn run() -> Result<(), String> {
        let ops = env::args().nth(1).map_or(Ok(DEFAULT_OPS), |raw| {
            raw.parse::<u64>().map_err(|error| error.to_string())
        })?;
        let (sources, derived): (Vec<Cell<i64, CellMutable>>, Vec<_>) = no_coalesce(|| {
            let sources: Vec<Cell<i64, CellMutable>> =
                (0..THREADS).map(|_| Cell::new(0_i64)).collect();
            let derived = sources
                .iter()
                .map(|source| {
                    source
                        .clone()
                        .map(|value| value.saturating_add(1))
                        .materialize()
                })
                .collect();
            (sources, derived)
        });
        let delivered: Vec<Arc<AtomicU64>> =
            (0..THREADS).map(|_| Arc::new(AtomicU64::new(0))).collect();
        let previous: Vec<Arc<AtomicI64>> =
            (0..THREADS).map(|_| Arc::new(AtomicI64::new(0))).collect();
        let mismatches: Vec<Arc<AtomicU64>> =
            (0..THREADS).map(|_| Arc::new(AtomicU64::new(0))).collect();
        let guards: Vec<_> = derived
            .iter()
            .zip(&delivered)
            .zip(previous.iter().zip(&mismatches))
            .map(|((cell, count), (prior, mismatch))| {
                let count = Arc::clone(count);
                let prior = Arc::clone(prior);
                let mismatch = Arc::clone(mismatch);
                cell.subscribe(move |signal| {
                    if let Signal::Value(value) = signal {
                        let old = prior.swap(**value, Ordering::Relaxed);
                        if **value != old.saturating_add(1) {
                            mismatch.fetch_add(1, Ordering::Relaxed);
                        }
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        for count in &delivered {
            count.store(0, Ordering::Relaxed);
        }
        for mismatch in &mismatches {
            mismatch.store(0, Ordering::Relaxed);
        }

        let started = Instant::now();
        thread::scope(|scope| {
            for (index, source) in sources.iter().enumerate() {
                scope.spawn(move || {
                    for i in 0..ops {
                        let value = i64::try_from(i.saturating_add(1)).unwrap_or(i64::MAX);
                        batch(|| source.set(black_box(value)));
                    }
                    black_box(index);
                });
            }
        });
        let elapsed = started.elapsed();

        let final_source = i64::try_from(ops).unwrap_or(i64::MAX);
        let final_derived = final_source.saturating_add(1);
        let counts: Vec<u64> = delivered
            .iter()
            .map(|value| value.load(Ordering::Relaxed))
            .collect();
        let order_failures: Vec<u64> = mismatches
            .iter()
            .map(|value| value.load(Ordering::Relaxed))
            .collect();
        if sources.iter().any(|source| source.get() != final_source) {
            return Err("Source final value differed".to_owned());
        }
        if derived.iter().any(|cell| cell.get() != final_derived) {
            return Err("Derived final value differed".to_owned());
        }
        if counts.iter().any(|count| *count != ops) {
            return Err(format!(
                "Expected {ops} deliveries per branch, got {counts:?}"
            ));
        }
        if order_failures.iter().any(|count| *count != 0) {
            return Err(format!("Payload sequence failures: {order_failures:?}"));
        }
        black_box(&guards);

        let first = counts.first().copied().unwrap_or(0);
        let second = counts.get(1).copied().unwrap_or(0);
        println!(
            "{{\"ops_per_thread\":{ops},\"elapsed_ns\":{},\"delivered\":[{first},{second}],\"total_delivered\":{},\"order_failures\":[{},{}]}}",
            elapsed.as_nanos(),
            counts.iter().sum::<u64>(),
            order_failures.first().copied().unwrap_or(0),
            order_failures.get(1).copied().unwrap_or(0)
        );
        Ok(())
    }
}

#[cfg(feature = "scheduler")]
fn main() -> Result<(), String> {
    probe::run()
}

#[cfg(not(feature = "scheduler"))]
fn main() {
    eprintln!("Run with --features scheduler");
}
