use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
};

use super::CellMap;
use crate::SubscriptionGuard;

#[test]
fn concurrent_owned_guards_live_until_the_final_map_handle_drops() {
    let map = CellMap::<u32, u32>::new();
    let final_handle = map.clone();
    let drops = Arc::new(
        (0..128)
            .map(|_| Arc::new(AtomicUsize::new(0)))
            .collect::<Vec<_>>(),
    );
    let start = Arc::new(Barrier::new(8));

    std::thread::scope(|scope| {
        for worker in 0..8 {
            let map = map.clone();
            let drops = drops.clone();
            let start = start.clone();
            scope.spawn(move || {
                start.wait();
                for count in drops.iter().skip(worker * 16).take(16) {
                    let count = count.clone();
                    map.own(SubscriptionGuard::from_callback(move || {
                        count.fetch_add(1, Ordering::SeqCst);
                    }));
                }
            });
        }
    });

    assert!(drops.iter().all(|count| count.load(Ordering::SeqCst) == 0));
    drop(map);
    assert!(drops.iter().all(|count| count.load(Ordering::SeqCst) == 0));
    drop(final_handle);
    assert!(drops.iter().all(|count| count.load(Ordering::SeqCst) == 1));
}

#[test]
fn owned_guard_cleanup_can_register_on_a_surviving_map() {
    let map = CellMap::<u32, u32>::new();
    let weak = map.downgrade();
    let survivor = CellMap::<u32, u32>::new();
    let cleanup_calls = Arc::new(AtomicUsize::new(0));
    let registered_drops = Arc::new(AtomicUsize::new(0));

    map.own(SubscriptionGuard::from_callback({
        let survivor = survivor.clone();
        let cleanup_calls = cleanup_calls.clone();
        let registered_drops = registered_drops.clone();
        move || {
            assert!(weak.upgrade().is_none());
            cleanup_calls.fetch_add(1, Ordering::SeqCst);
            let registered_drops = registered_drops.clone();
            survivor.own(SubscriptionGuard::from_callback(move || {
                registered_drops.fetch_add(1, Ordering::SeqCst);
            }));
        }
    }));

    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 0);
    drop(map);
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
    assert_eq!(registered_drops.load(Ordering::SeqCst), 0);
    drop(survivor);
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
    assert_eq!(registered_drops.load(Ordering::SeqCst), 1);
}
