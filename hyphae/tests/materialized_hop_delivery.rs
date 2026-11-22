//! A materialized `switch_map`, `join`, `join_vec` or `merge_map` delivers a
//! changed value to its subscriber, both synchronously and under `batch`.
//!
//! `batch` joins one process-wide tick, so these tests hold `SERIAL` to keep
//! another test's batch from deferring their synchronous writes.

use std::sync::Arc;

use hyphae::{
    Cell, Gettable, JoinExt, Materialize, MergeMapExt, Mutable, SwitchMapExt, Watchable, batch,
    join_vec,
};
use parking_lot::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> parking_lot::MutexGuard<'static, ()> {
    SERIAL.lock()
}

fn last<T: Clone + Send + Sync + 'static>(seen: &Arc<Mutex<Vec<T>>>) -> Option<T> {
    seen.lock().last().cloned()
}

fn recorder<T: Clone + Send + Sync + 'static>()
-> (Arc<Mutex<Vec<T>>>, impl Fn(&Arc<T>) + Send + Sync + 'static) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    (seen, move |v: &Arc<T>| s.lock().push((**v).clone()))
}

#[test]
fn switch_map_delivers_inner_and_switch_writes() {
    let _serial = serial();
    let outer = Cell::new(0u64);
    let even = Cell::new(10u64);
    let odd = Cell::new(20u64);
    let (e, o) = (even.clone(), odd);
    let out = outer
        .clone()
        .switch_map(move |n| if n % 2 == 0 { e.clone() } else { o.clone() })
        .materialize();
    let (seen, f) = recorder();
    let _g = out.subscribe(move |s| {
        if let hyphae::Signal::Value(v) = s {
            f(v);
        }
    });

    even.set(11);
    assert_eq!(last(&seen), Some(11));
    batch(|| even.set(12));
    assert_eq!(last(&seen), Some(12));
    outer.set(1);
    assert_eq!(last(&seen), Some(20));
    batch(|| outer.set(2));
    assert_eq!(last(&seen), Some(12));
    assert_eq!(out.get(), 12);
}

#[test]
fn join_delivers_writes() {
    let _serial = serial();
    let a = Cell::new(1u64);
    let b = Cell::new(2u64);
    let out = a.clone().join(b.clone()).materialize();
    let (seen, f) = recorder();
    let _g = out.subscribe(move |s| {
        if let hyphae::Signal::Value(v) = s {
            f(v);
        }
    });
    a.set(3);
    assert_eq!(last(&seen), Some((3, 2)));
    batch(|| b.set(4));
    assert_eq!(last(&seen), Some((3, 4)));
}

#[test]
fn join_vec_delivers_writes() {
    let _serial = serial();
    let cells = [Cell::new(1u64), Cell::new(2u64), Cell::new(3u64)];
    let out = join_vec(cells.to_vec()).materialize();
    let (seen, f) = recorder();
    let _g = out.subscribe(move |s| {
        if let hyphae::Signal::Value(v) = s {
            f(v);
        }
    });
    cells[1].set(20);
    assert_eq!(last(&seen), Some(vec![1, 20, 3]));
    batch(|| cells[1].set(21));
    assert_eq!(last(&seen), Some(vec![1, 21, 3]));
}

#[test]
fn merge_map_delivers_writes() {
    let _serial = serial();
    let outer = Cell::new(0u64);
    let inner = Cell::new(10u64);
    let i = inner.clone();
    let out = outer.merge_map(move |_| i.clone()).materialize();
    let (seen, f) = recorder();
    let _g = out.subscribe(move |s| {
        if let hyphae::Signal::Value(v) = s {
            f(v);
        }
    });
    inner.set(11);
    assert_eq!(last(&seen), Some(11));
    batch(|| inner.set(12));
    assert_eq!(last(&seen), Some(12));
}
