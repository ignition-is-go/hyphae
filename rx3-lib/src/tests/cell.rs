use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::{Cell, DepNode, Gettable, Mutable, Signal};
use crate::traits::Watchable;

#[test]
fn test_cell_new_and_get() {
    let cell = Cell::new(42);
    assert_eq!(cell.get(), 42);
}

#[test]
fn test_cell_set() {
    let cell = Cell::new(0);
    cell.set(100);
    assert_eq!(cell.get(), 100);
}

#[test]
fn test_cell_watch_immediate() {
    let cell = Cell::new(42u64);
    let received = Arc::new(AtomicU64::new(0));

    let r = received.clone();
    let _guard = cell.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            r.store(*v, Ordering::SeqCst);
        }
    });

    // Watch should call immediately with current value
    assert_eq!(received.load(Ordering::SeqCst), 42);
}

#[test]
fn test_cell_watch_on_set() {
    let cell = Cell::new(0u64);
    let received = Arc::new(AtomicU64::new(0));

    let r = received.clone();
    let _guard = cell.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            r.store(*v, Ordering::SeqCst);
        }
    });

    cell.set(99);
    assert_eq!(received.load(Ordering::SeqCst), 99);
}

#[test]
fn test_cell_multiple_watchers() {
    let cell = Cell::new(0u64);
    let count = Arc::new(AtomicU64::new(0));

    let _guards: Vec<_> = (0..10).map(|_| {
        let c = count.clone();
        cell.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                c.fetch_add(1, Ordering::SeqCst);
            }
        })
    }).collect();

    // 10 watchers called immediately
    assert_eq!(count.load(Ordering::SeqCst), 10);

    cell.set(1);
    // 10 more calls
    assert_eq!(count.load(Ordering::SeqCst), 20);
}

#[test]
fn test_cell_unsubscribe() {
    let cell = Cell::new(0u64);
    let count = Arc::new(AtomicU64::new(0));

    let c = count.clone();
    let guard = cell.subscribe(move |signal| {
        if let Signal::Value(_) = signal {
            c.fetch_add(1, Ordering::SeqCst);
        }
    });

    assert_eq!(count.load(Ordering::SeqCst), 1); // immediate call

    cell.set(1);
    assert_eq!(count.load(Ordering::SeqCst), 2);

    drop(guard); // unsubscribe by dropping the guard

    cell.set(2);
    assert_eq!(count.load(Ordering::SeqCst), 2); // no change after unsubscribe
}

#[test]
fn test_cell_with_name() {
    let cell = Cell::new(42).with_name("my_cell");
    assert_eq!(cell.name(), Some("my_cell".to_string()));
}

#[test]
fn test_cell_clone_shares_state() {
    let cell1 = Cell::new(0);
    let cell2 = cell1.clone();

    cell1.set(42);
    assert_eq!(cell2.get(), 42);
}

#[test]
fn test_cell_dep_node() {
    let cell = Cell::new(42).with_name("test");

    assert!(cell.name().is_some());
    assert_eq!(cell.dependency_count(), 0);
    assert!(!cell.has_dependencies());
}
