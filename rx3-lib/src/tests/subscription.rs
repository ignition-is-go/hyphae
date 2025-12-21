use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::{Cell, Mutable, Watchable};

#[test]
fn test_subscribe_returns_guard() {
    let source = Cell::new(0u64);
    let received = Arc::new(AtomicU64::new(0));

    let r = received.clone();
    let guard = source.subscribe(move |v| {
        r.store(*v, Ordering::SeqCst);
    });

    assert_eq!(received.load(Ordering::SeqCst), 0);

    source.set(42);
    assert_eq!(received.load(Ordering::SeqCst), 42);

    // Guard is still active
    assert_eq!(guard.id(), guard.id()); // just accessing id
}

#[test]
fn test_guard_unsubscribes_on_drop() {
    let source = Cell::new(0u64);
    let received = Arc::new(AtomicU64::new(0));

    {
        let r = received.clone();
        let _guard = source.subscribe(move |v| {
            r.store(*v, Ordering::SeqCst);
        });

        source.set(1);
        assert_eq!(received.load(Ordering::SeqCst), 1);
    } // guard dropped here

    // After drop, updates should not reach callback
    source.set(2);
    assert_eq!(received.load(Ordering::SeqCst), 1); // still 1
}

#[test]
fn test_guard_leak_prevents_unsubscribe() {
    let source = Cell::new(0u64);
    let received = Arc::new(AtomicU64::new(0));

    let r = received.clone();
    let guard = source.subscribe(move |v| {
        r.store(*v, Ordering::SeqCst);
    });

    let _id = guard.leak(); // prevent auto-unsubscribe

    source.set(42);
    assert_eq!(received.load(Ordering::SeqCst), 42); // still works
}

#[test]
fn test_guard_manual_unsubscribe() {
    let source = Cell::new(0u64);
    let received = Arc::new(AtomicU64::new(0));

    let r = received.clone();
    let guard = source.subscribe(move |v| {
        r.store(*v, Ordering::SeqCst);
    });

    source.set(1);
    assert_eq!(received.load(Ordering::SeqCst), 1);

    guard.unsubscribe();

    source.set(2);
    assert_eq!(received.load(Ordering::SeqCst), 1); // still 1
}
