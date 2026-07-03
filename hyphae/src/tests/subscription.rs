use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::{Cell, Mutable, Signal, Watchable};

#[test]
fn test_subscribe_returns_guard() {
    let source = Cell::new(0u64);
    let received = Arc::new(AtomicU64::new(0));

    let r = received.clone();
    let guard = source.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            r.store(**v, Ordering::SeqCst);
        }
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
        let _guard = source.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                r.store(**v, Ordering::SeqCst);
            }
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
    let guard = source.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            r.store(**v, Ordering::SeqCst);
        }
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
    let guard = source.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            r.store(**v, Ordering::SeqCst);
        }
    });

    source.set(1);
    assert_eq!(received.load(Ordering::SeqCst), 1);

    guard.unsubscribe();

    source.set(2);
    assert_eq!(received.load(Ordering::SeqCst), 1); // still 1
}

// The indexed subscriber registry keeps a cached notify snapshot that is
// rebuilt lazily on the next notify after any subscribe/unsubscribe. This
// exercises that path directly: an unsubscribe between two notifies must drop
// its subscriber from the rebuilt snapshot, and a surviving subscriber must
// keep firing.
#[test]
fn test_indexed_registry_rebuilds_snapshot_after_unsubscribe() {
    let source = Cell::new(0u64);
    let a_hits = Arc::new(AtomicU64::new(0));
    let b_hits = Arc::new(AtomicU64::new(0));

    let a = a_hits.clone();
    let guard_a = source.subscribe(move |signal| {
        if let Signal::Value(_) = signal {
            a.fetch_add(1, Ordering::SeqCst);
        }
    });
    let b = b_hits.clone();
    let _guard_b = source.subscribe(move |signal| {
        if let Signal::Value(_) = signal {
            b.fetch_add(1, Ordering::SeqCst);
        }
    });

    // First notify builds the snapshot with both subscribers.
    source.set(1);
    assert_eq!(a_hits.load(Ordering::SeqCst), 2); // initial subscribe + set(1)
    assert_eq!(b_hits.load(Ordering::SeqCst), 2);

    // Drop A between notifies. The next notify must rebuild the snapshot
    // without A while continuing to fire B.
    drop(guard_a);
    source.set(2);
    assert_eq!(a_hits.load(Ordering::SeqCst), 2); // unchanged — A is gone
    assert_eq!(b_hits.load(Ordering::SeqCst), 3); // B still firing
}

// Churn many distinct subscriptions on one cell (the switch_map-style
// resubscribe-per-fire pattern that dominated the profile). Correctness must
// hold: after the storm only the still-held guards fire, exactly once each.
#[test]
fn test_indexed_registry_survives_subscription_churn() {
    let source = Cell::new(0u64);
    let survivor_hits = Arc::new(AtomicU64::new(0));

    let s = survivor_hits.clone();
    let _survivor = source.subscribe(move |signal| {
        if let Signal::Value(_) = signal {
            s.fetch_add(1, Ordering::SeqCst);
        }
    });
    let baseline = survivor_hits.load(Ordering::SeqCst); // 1, from initial

    // Repeatedly subscribe a transient observer and drop it, driving a notify
    // each round — the exact subscribe → notify → unsubscribe churn.
    for i in 0..1_000u64 {
        let transient_hits = Arc::new(AtomicU64::new(0));
        let t = transient_hits.clone();
        let guard = source.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                t.fetch_add(1, Ordering::SeqCst);
            }
        });
        source.set(i);
        drop(guard);
        // The transient saw its initial value plus the one set while subscribed.
        assert_eq!(transient_hits.load(Ordering::SeqCst), 2);
    }

    // The survivor fired once per `set` (1000) plus its initial subscribe.
    assert_eq!(
        survivor_hits.load(Ordering::SeqCst),
        baseline + 1_000,
        "survivor must be notified on every set across the churn"
    );
}
