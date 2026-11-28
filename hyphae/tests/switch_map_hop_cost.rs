//! Fanout census for `switch_map` hops (`profiling` feature).
//!
//! Each fanout is one scheduled op under `batch`, so the count per source write
//! is the hop's cost on the scheduler queue. An operator materialized directly
//! costs one fanout per delivered value. A trailing `map`/`deduped` keeps its
//! own cell, so it runs once per settled value rather than per upstream notify.
//!
//! `fanouts` runs its write twice, so every write produces a new value: a
//! repeated value is dropped at a deduping cell and would count nothing.
#![cfg(feature = "profiling")]

use hyphae::{
    Cell, DedupedExt, Gettable, JoinExt, MapExt, Materialize, MergeMapExt, Mutable, SwitchMapExt,
    Watchable, batch, join_vec,
    profiling::{pass, take_report},
};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Fanouts across every cell for one `write`, synchronously and under `batch`.
fn fanouts(write: impl Fn()) -> (u64, u64) {
    pass(&write);
    let sync = take_report().map_or(0, |r| r.total_fires());
    pass(|| batch(&write));
    let batched = take_report().map_or(0, |r| r.total_fires());
    (sync, batched)
}

#[test]
fn switch_map_hop_costs_one_fanout_per_value() {
    let _serial = serial();
    let outer = Cell::new(0u64);
    let even = Cell::new(10u64);
    let odd = Cell::new(1_000u64);
    let (e, o) = (even.clone(), odd);
    let out = outer
        .clone()
        .switch_map(move |n| if n % 2 == 0 { e.clone() } else { o.clone() })
        .materialize();
    let _g = out.subscribe(|_| {});

    let inner_write = fanouts(|| even.set(even.get() + 1));
    // Each switch lands on the other inner, so the output really changes and
    // a deduping cell cannot drop it.
    let outer_write = fanouts(|| outer.set(outer.get() + 1));
    // Source fanout + exactly one downstream fanout.
    assert_eq!(inner_write, (2, 2), "inner write");
    assert_eq!(outer_write, (2, 2), "outer write (switch)");
    assert_eq!(out.get(), even.get());
}

#[test]
fn switch_map_with_trailing_map_and_deduped_keeps_a_settling_cell() {
    let _serial = serial();
    let outer = Cell::new(0u64);
    let inner = Cell::new(10u64);
    let i = inner.clone();
    let out = outer
        .switch_map(move |_| i.clone())
        .map(|v| v * 2)
        .deduped()
        .materialize();
    let _g = out.subscribe(|_| {});

    let inner_write = fanouts(|| inner.set(inner.get() + 1));
    assert_eq!(inner_write, (3, 3), "inner write through map+deduped");
    assert_eq!(out.get(), inner.get() * 2);
}

#[test]
fn nested_switch_map_costs_one_fanout_per_hop() {
    let _serial = serial();
    let a = Cell::new(0u64);
    let b = Cell::new(0u64);
    let c = Cell::new(100u64);
    let cc = c.clone();
    let out = a
        .switch_map(move |_| {
            let cc = cc.clone();
            b.clone().switch_map(move |_| cc.clone()).materialize()
        })
        .materialize();
    let _g = out.subscribe(|_| {});

    // c -> inner switch -> outer switch: two switch hops.
    let leaf_write = fanouts(|| c.set(c.get() + 1));
    assert_eq!(leaf_write, (3, 3), "leaf write through two switch hops");
    assert_eq!(out.get(), c.get());
}

#[test]
fn join_costs_one_fanout() {
    let _serial = serial();
    let a = Cell::new(1u64);
    let b = Cell::new(2u64);
    let out = a.clone().join(b.clone()).materialize();
    let _g = out.subscribe(|_| {});
    assert_eq!(fanouts(|| a.set(a.get() + 1)), (2, 2), "left write");
    assert_eq!(fanouts(|| b.set(b.get() + 1)), (2, 2), "right write");
    assert_eq!(out.get(), (a.get(), b.get()));
}

#[test]
fn join_vec_costs_one_fanout() {
    let _serial = serial();
    let cells = [Cell::new(1u64), Cell::new(2u64), Cell::new(3u64)];
    let out = join_vec(cells.to_vec()).materialize();
    let _g = out.subscribe(|_| {});
    assert_eq!(
        fanouts(|| cells[1].set(cells[1].get() + 1)),
        (2, 2),
        "member write"
    );
    assert_eq!(out.get(), vec![1, 4, 3]);
}

#[test]
fn merge_map_costs_one_fanout() {
    let _serial = serial();
    let outer = Cell::new(0u64);
    let inner = Cell::new(10u64);
    let i = inner.clone();
    let out = outer.merge_map(move |_| i.clone()).materialize();
    let _g = out.subscribe(|_| {});
    assert_eq!(
        fanouts(|| inner.set(inner.get() + 1)),
        (2, 2),
        "inner write"
    );
    assert_eq!(out.get(), 12);
}
