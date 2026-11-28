//! Write-path benches: what one write costs on each route into propagation.
//!
//! - `cell_batch/eight_writes` — eight source cells written in one batch,
//!   each with one subscriber. With `scheduler`'s `single_cell/synchronous`
//!   and `diamond/batched` this covers the plain-cell routes.
//! - `map_write/*` — a `CellMap` feeding one materialized `select`, so every
//!   write stores a row, emits a diff and updates a derived map.
//!   `unbatched_insert` is one insert outside a batch. `batched_inserts`
//!   is eight inserts in one batch on one thread. The `two_threads` cases
//!   run that batch on two threads at once, on disjoint keys and on the same
//!   keys; the sample is the wall time for both threads to finish, divided by
//!   the batches each ran.

use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Instant,
};

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use hyphae::{Cell, CellMap, MapQuery, Mutable, SelectExt, SubscriptionGuard, Watchable, batch};

const WRITES: usize = 8;

fn bench_cell_batch(c: &mut Criterion) {
    let hits = Arc::new(AtomicU64::new(0));
    let cells: Vec<_> = (0..WRITES).map(|_| Cell::new(0i64)).collect();
    let _guards: Vec<SubscriptionGuard> = cells
        .iter()
        .map(|cell| {
            let hits = Arc::clone(&hits);
            cell.subscribe(move |_| {
                hits.fetch_add(1, Ordering::Relaxed);
            })
        })
        .collect();

    let mut i = 0i64;
    c.bench_function("cell_batch/eight_writes", |b| {
        b.iter(|| {
            i = i.saturating_add(1);
            batch(|| {
                for cell in &cells {
                    cell.set(black_box(i));
                }
            });
        });
    });
}

struct Graph {
    source: CellMap<u64, i64>,
    derived: CellMap<u64, i64, hyphae::CellImmutable>,
}

impl Graph {
    /// Print how many rows of the derived map differ from the source, so a
    /// run shows that the timed writes were propagated and where they ended.
    fn report(&self, name: &str) {
        let mut source = self.source.snapshot();
        let mut derived = self.derived.snapshot();
        source.sort_unstable();
        derived.sort_unstable();
        let differing = source
            .iter()
            .zip(&derived)
            .filter(|(left, right)| left != right)
            .count()
            .saturating_add(source.len().abs_diff(derived.len()));
        eprintln!(
            "map_write/{name}: {} rows, {differing} differ in the derived map",
            source.len()
        );
    }
}

fn graph() -> Graph {
    let source = CellMap::<u64, i64>::new();
    let derived = source.clone().select(|_| true).materialize();
    Graph { source, derived }
}

fn insert_batch(source: &CellMap<u64, i64>, first_key: u64, value: i64) {
    batch(|| {
        for offset in 0..WRITES {
            let key = first_key.saturating_add(u64::try_from(offset).unwrap_or(0));
            black_box(source.insert(key, black_box(value)));
        }
    });
}

/// Run `insert_batch` `iters` times on each of two threads and return the
/// wall time from their common start to the later finish.
fn two_threads(graph: &Graph, second_first_key: u64, iters: u64) -> std::time::Duration {
    let start = Barrier::new(3);
    thread::scope(|scope| {
        for first_key in [0, second_first_key] {
            let (source, start) = (&graph.source, &start);
            scope.spawn(move || {
                start.wait();
                for i in 1..=iters {
                    insert_batch(source, first_key, i64::try_from(i).unwrap_or(0));
                }
            });
        }
        start.wait();
        let began = Instant::now();
        // The scope joins both writers before it returns.
        began
    })
    .elapsed()
}

fn bench_map_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("map_write");

    let unbatched = graph();
    let mut i = 0i64;
    group.bench_function("unbatched_insert", |b| {
        b.iter(|| {
            i = i.saturating_add(1);
            black_box(unbatched.source.insert(0, black_box(i)));
        });
    });
    unbatched.report("unbatched_insert");

    let batched = graph();
    let mut i = 0i64;
    group.bench_function("batched_inserts", |b| {
        b.iter(|| {
            i = i.saturating_add(1);
            insert_batch(&batched.source, 0, i);
        });
    });
    batched.report("batched_inserts");

    let disjoint = graph();
    group.bench_function("two_threads/disjoint_keys", |b| {
        b.iter_custom(|iters| two_threads(&disjoint, 1_000, iters));
    });
    disjoint.report("two_threads/disjoint_keys");

    let same = graph();
    group.bench_function("two_threads/same_keys", |b| {
        b.iter_custom(|iters| two_threads(&same, 0, iters));
    });
    same.report("two_threads/same_keys");

    group.finish();
}

criterion_group!(benches, bench_cell_batch, bench_map_write);
criterion_main!(benches);
