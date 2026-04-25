use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::{Cell, Gettable, MapExt, Mutable, Pipeline, Signal, traits::Watchable};

// ============================================================================
// WeakCell Tests
// ============================================================================

#[test]
fn test_weak_cell_upgrade_succeeds_while_cell_alive() {
    let cell = Cell::new(42u64);
    let weak = cell.downgrade();

    // While cell is alive, upgrade should succeed
    let upgraded = weak.upgrade();
    assert!(upgraded.is_some());
    assert_eq!(upgraded.unwrap().get(), 42);
}

#[test]
fn test_weak_cell_upgrade_fails_after_drop() {
    let weak = {
        let cell = Cell::new(42u64);
        cell.downgrade()
    }; // cell is dropped here

    // After cell is dropped, upgrade should fail
    assert!(weak.upgrade().is_none());
}

#[test]
fn test_weak_cell_clone() {
    let cell = Cell::new(42u64);
    let weak1 = cell.downgrade();
    let weak2 = weak1.clone();

    // Both should upgrade successfully
    assert!(weak1.upgrade().is_some());
    assert!(weak2.upgrade().is_some());

    drop(cell);

    // Both should fail after drop
    assert!(weak1.upgrade().is_none());
    assert!(weak2.upgrade().is_none());
}

// ============================================================================
// Disposal Behavior Tests
// ============================================================================

#[test]
fn test_derived_cell_drop_stops_notifications() {
    let source = Cell::new(0u64);
    let call_count = Arc::new(AtomicU64::new(0));

    {
        let count = call_count.clone();
        let _derived = source.clone().map(move |v| {
            count.fetch_add(1, Ordering::SeqCst);
            *v * 2
        }).materialize();

        // Under the fused-pipeline model, materialize() calls the map closure
        // twice on creation: once for `self.get()` to compute the initial cell
        // value, and once when the install() subscription fires synchronously
        // with the current source value. (Notification of the cell is suppressed
        // for the second call, but the map closure itself still runs.)
        let after_create = call_count.load(Ordering::SeqCst);
        assert!(after_create >= 1);

        source.set(1);
        // Map callback called at least once more for the new value
        assert_eq!(call_count.load(Ordering::SeqCst), after_create + 1);
    } // derived is dropped here

    let after_drop = call_count.load(Ordering::SeqCst);
    // After dropping derived, setting source should NOT trigger the map callback
    source.set(2);
    assert_eq!(call_count.load(Ordering::SeqCst), after_drop); // Not incremented
}

#[test]
fn test_source_still_works_after_derived_dropped() {
    let source = Cell::new(0u64);
    let source_call_count = Arc::new(AtomicU64::new(0));

    // Add a direct watcher on source
    let count = source_call_count.clone();
    let _guard = source.subscribe(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
    });

    {
        let _derived = source.clone().map(|v| *v * 2).materialize();
        source.set(1);
    } // derived dropped

    // Source should still work for other watchers
    source.set(2);
    // Initial watch (0) + set(1) + set(2) = 3 calls
    assert_eq!(source_call_count.load(Ordering::SeqCst), 3);
}

#[test]
fn test_chained_operators_drop_correctly() {
    let source = Cell::new(0u64);
    let map1_count = Arc::new(AtomicU64::new(0));
    let map2_count = Arc::new(AtomicU64::new(0));

    let c1 = map1_count.clone();
    let c2 = map2_count.clone();

    {
        let _final = source
            .clone()
            .map(move |v| {
                c1.fetch_add(1, Ordering::SeqCst);
                *v * 2
            })
            .map(move |v| {
                c2.fetch_add(1, Ordering::SeqCst);
                *v + 1
            })
            .materialize();

        // Under the fused-pipeline model, materialize() runs the fused closure
        // twice on creation: once for `self.get()` to compute the initial cell
        // value, and once when the install() subscription fires synchronously.
        let m1_after_create = map1_count.load(Ordering::SeqCst);
        let m2_after_create = map2_count.load(Ordering::SeqCst);
        assert!(m1_after_create >= 1);
        assert!(m2_after_create >= 1);

        source.set(1);
        assert_eq!(map1_count.load(Ordering::SeqCst), m1_after_create + 1);
        assert_eq!(map2_count.load(Ordering::SeqCst), m2_after_create + 1);
    } // both derived cells dropped

    let m1_after_drop = map1_count.load(Ordering::SeqCst);
    let m2_after_drop = map2_count.load(Ordering::SeqCst);
    source.set(2);
    // Neither map should be called anymore
    assert_eq!(map1_count.load(Ordering::SeqCst), m1_after_drop);
    assert_eq!(map2_count.load(Ordering::SeqCst), m2_after_drop);
}

#[test]
fn test_parent_cell_outlives_derived() {
    let received = Arc::new(AtomicU64::new(0));
    let r = received.clone();

    let source = Cell::new(0u64);
    let derived = source.clone().map(|v| *v * 2).materialize();

    let _guard = derived.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            r.store(**v, Ordering::SeqCst);
        }
    });

    source.set(5);
    assert_eq!(received.load(Ordering::SeqCst), 10);

    // Derived still holds strong ref to source via dependencies,
    // so source stays alive even if we drop our reference
    drop(source);

    // derived should still have its value
    assert_eq!(derived.get(), 10);
}

// ============================================================================
// Subscription Cleanup Tests
// ============================================================================

#[test]
fn test_subscription_cleaned_up_on_derived_drop() {
    let source = Cell::new(0u64);

    // Check initial subscriber count
    let initial_count = source.inner.subscribers.load().len();

    {
        let _derived = source.clone().map(|v| *v * 2).materialize();
        // map() creates one subscription on source
        assert_eq!(source.inner.subscribers.load().len(), initial_count + 1);
    } // derived dropped here - should unsubscribe

    // Subscription should be cleaned up
    assert_eq!(source.inner.subscribers.load().len(), initial_count);
}

#[test]
fn test_chained_subscriptions_cleaned_up() {
    let source = Cell::new(0u64);
    let initial_count = source.inner.subscribers.load().len();

    {
        let derived1 = source.clone().map(|v| *v * 2).materialize();
        let d1_initial = derived1.inner.subscribers.load().len();

        {
            let _derived2 = derived1.clone().map(|v| *v + 1).materialize();
            // derived2 subscribes to derived1
            assert_eq!(derived1.inner.subscribers.load().len(), d1_initial + 1);
        } // derived2 dropped

        // derived1 subscription should be cleaned up
        assert_eq!(derived1.inner.subscribers.load().len(), d1_initial);
    } // derived1 dropped

    // source subscription should be cleaned up
    assert_eq!(source.inner.subscribers.load().len(), initial_count);
}

#[test]
fn test_multiple_derived_cells_independent_cleanup() {
    let source = Cell::new(0u64);
    let initial_count = source.inner.subscribers.load().len();

    let derived1 = source.clone().map(|v| *v * 2).materialize();
    assert_eq!(source.inner.subscribers.load().len(), initial_count + 1);

    let derived2 = source.clone().map(|v| *v + 1).materialize();
    assert_eq!(source.inner.subscribers.load().len(), initial_count + 2);

    drop(derived1);
    // Only derived1's subscription should be cleaned up
    assert_eq!(source.inner.subscribers.load().len(), initial_count + 1);

    drop(derived2);
    // Now both should be cleaned up
    assert_eq!(source.inner.subscribers.load().len(), initial_count);
}
