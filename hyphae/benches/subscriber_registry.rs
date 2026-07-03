//! Subscribe/unsubscribe scaling on a cell that already has N subscribers.
//!
//! This isolates the cost the HRLV CPU profile flagged: the copy-on-write
//! `eq<Uuid>` scan that the previous `Arc<Vec>` registry paid on every
//! subscribe (full clone) and unsubscribe (linear filter). With the id-indexed
//! registry both are O(1), so churn time should stay flat as N grows instead of
//! scaling linearly.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use hyphae::{Cell, Mutable, Watchable};

/// Churn a transient subscription (subscribe then drop) against a cell already
/// holding `n` stable subscribers. No notify runs between the subscribe and the
/// drop, so this measures the registry mutation cost in isolation — O(n) per op
/// under the old COW `Vec`, O(1) under the index.
fn bench_churn_against_n_subscribers(c: &mut Criterion) {
    let mut group = c.benchmark_group("subscribe+unsubscribe churn");
    for n in [16usize, 256, 4096] {
        let cell = Cell::new(0u64);
        // Hold N stable subscribers for the whole measurement.
        let mut stable = Vec::with_capacity(n);
        for _ in 0..n {
            stable.push(cell.subscribe(|_| {}));
        }

        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let guard = cell.subscribe(|_| {});
                black_box(&guard);
                drop(guard);
            });
        });

        drop(stable);
    }
    group.finish();
}

/// Full fire cycle — subscribe a transient, `set` (one notify), drop it —
/// against `n` stable subscribers. This is the switch_map-style
/// resubscribe-per-fire shape end to end (the index removes two O(n) COW copies
/// per fire; one O(n) snapshot rebuild remains, amortized into the notify).
fn bench_resubscribe_per_fire(c: &mut Criterion) {
    let mut group = c.benchmark_group("resubscribe per fire");
    for n in [16usize, 256, 4096] {
        let cell = Cell::new(0u64);
        let mut stable = Vec::with_capacity(n);
        for _ in 0..n {
            stable.push(cell.subscribe(|_| {}));
        }

        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            let mut v = 0u64;
            b.iter(|| {
                let guard = cell.subscribe(|_| {});
                v = v.wrapping_add(1);
                cell.set(black_box(v));
                drop(guard);
            });
        });

        drop(stable);
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_churn_against_n_subscribers,
    bench_resubscribe_per_fire
);
criterion_main!(benches);
