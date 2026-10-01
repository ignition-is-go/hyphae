#![cfg(feature = "scheduler")]

use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use hyphae::{
    Cell, CellMutable, DepNode, Gettable, Mutable, Signal, SubscriptionGuard, Watchable, batch,
    cell::WeakCell, scheduler::no_coalesce,
};

static TEST_TICK: Mutex<()> = Mutex::new(());

fn scheduler_atomics(cell: &Cell<u64, CellMutable>) -> Option<(&AtomicU64, &AtomicU64)> {
    cell.height_cache().zip(cell.height_epoch())
}

#[derive(Clone)]
struct ReentrantValue {
    value: u64,
    cleanup: Option<Arc<ReentrantCleanup>>,
}

impl ReentrantValue {
    const fn plain(value: u64) -> Self {
        Self {
            value,
            cleanup: None,
        }
    }
}

impl fmt::Debug for ReentrantValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReentrantValue")
            .field("value", &self.value)
            .field("has_cleanup", &self.cleanup.is_some())
            .finish()
    }
}

impl PartialEq for ReentrantValue {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

struct ReentrantCleanup {
    calls: Arc<AtomicUsize>,
    temporary_weak: Arc<Mutex<Option<WeakCell<u64, CellMutable>>>>,
    probe: Cell<u64, CellMutable>,
}

impl Drop for ReentrantCleanup {
    fn drop(&mut self) {
        batch(|| {
            self.probe.set(99);
            let temporary = Cell::new(1_u64);
            let weak = temporary.downgrade();
            temporary.set(2);
            drop(temporary);
            assert!(
                weak.is_alive(),
                "queued work must retain its captured Cell until the outer drain"
            );
            *self
                .temporary_weak
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(weak);
        });
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn cached_height_references_stay_stable_across_unrelated_arena_churn() {
    let _tick = TEST_TICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let anchor = Cell::new(7_u64);
    let atomics = scheduler_atomics(&anchor);
    assert!(
        atomics.is_some(),
        "scheduler cells must expose height atomics"
    );
    let Some((cache, epoch)) = atomics else {
        return;
    };
    let cache_address = std::ptr::from_ref(cache);
    let epoch_address = std::ptr::from_ref(epoch);
    cache.store(0x1234_5678, Ordering::Relaxed);
    let original_epoch = epoch.load(Ordering::Relaxed);

    for value in 0_u64..20_000 {
        let transient = Cell::new(value);
        let atomics = scheduler_atomics(&transient);
        assert!(
            atomics.is_some(),
            "scheduler cells must expose height atomics"
        );
        let Some((transient_cache, transient_epoch)) = atomics else {
            return;
        };
        assert_eq!(transient_cache.load(Ordering::Relaxed), 0);
        assert_eq!(transient_epoch.load(Ordering::Relaxed), 1);
        drop(transient);
    }

    let atomics_after = scheduler_atomics(&anchor);
    assert!(
        atomics_after.is_some(),
        "scheduler cells must retain height atomics"
    );
    let Some((cache_after, epoch_after)) = atomics_after else {
        return;
    };
    assert!(std::ptr::eq(cache_address, std::ptr::from_ref(cache_after)));
    assert!(std::ptr::eq(epoch_address, std::ptr::from_ref(epoch_after)));
    assert_eq!(cache_after.load(Ordering::Relaxed), 0x1234_5678);
    assert_eq!(epoch_after.load(Ordering::Relaxed), original_epoch);
}

#[test]
fn last_strong_drop_is_precise_and_reused_slots_do_not_resurrect_old_weaks() {
    let _tick = TEST_TICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original = Cell::new(11_u64);
    let weak = original.downgrade();
    let last = original.clone();
    drop(original);
    assert!(weak.is_alive());
    assert_eq!(weak.upgrade().map(|cell| cell.get()), Some(11));

    drop(last);
    assert!(!weak.is_alive());
    assert!(weak.upgrade().is_none());

    for value in 0_u64..20_000 {
        let replacement = Cell::new(value);
        let atomics = scheduler_atomics(&replacement);
        assert!(
            atomics.is_some(),
            "scheduler cells must expose height atomics"
        );
        let Some((cache, epoch)) = atomics else {
            return;
        };
        assert_eq!(cache.load(Ordering::Relaxed), 0);
        assert_eq!(epoch.load(Ordering::Relaxed), 1);
        cache.store(value.wrapping_add(1), Ordering::Relaxed);
        drop(replacement);
        assert!(!weak.is_alive(), "a freed slot resurrected an old WeakCell");
        assert!(weak.upgrade().is_none());
    }
}

#[test]
fn topology_rewiring_invalidates_cached_heights_through_the_dependent_cone() {
    let _tick = TEST_TICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let leaf = Cell::new(0_u64);
    let middle = Cell::new(0_u64);
    let owner = Cell::new(0_u64);
    owner.own(middle.subscribe(|_| {}));

    batch(|| owner.set(1));
    let atomics = scheduler_atomics(&owner);
    assert!(
        atomics.is_some(),
        "scheduler cells must expose height atomics"
    );
    let Some((owner_cache, owner_epoch)) = atomics else {
        return;
    };
    let initial_owner_epoch = owner_epoch.load(Ordering::Relaxed);
    assert_eq!(owner_cache.load(Ordering::Relaxed) & 0xFFFF_FFFF, 1);

    middle.own(leaf.subscribe(|_| {}));
    assert!(
        owner_epoch.load(Ordering::Relaxed) > initial_owner_epoch,
        "an upstream edge change did not invalidate its live dependent"
    );

    batch(|| owner.set(2));
    assert_eq!(owner_cache.load(Ordering::Relaxed) & 0xFFFF_FFFF, 2);
    assert_eq!(owner.get(), 2);
}

#[test]
fn owned_guard_cleanup_can_allocate_and_drop_cells_reentrantly() {
    let _tick = TEST_TICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cleanup_count = Arc::new(AtomicUsize::new(0));
    let cleanup_count_for_guard = cleanup_count.clone();
    let owner = Cell::new(0_u64);
    owner.own(SubscriptionGuard::from_callback(move || {
        for value in 0_u64..4_096 {
            let temporary = Cell::new(value);
            let atomics = scheduler_atomics(&temporary);
            assert!(
                atomics.is_some(),
                "scheduler cells must expose height atomics"
            );
            let Some((cache, epoch)) = atomics else {
                return;
            };
            assert_eq!(cache.load(Ordering::Relaxed), 0);
            assert_eq!(epoch.load(Ordering::Relaxed), 1);
            drop(temporary);
        }
        cleanup_count_for_guard.fetch_add(1, Ordering::SeqCst);
    }));

    drop(owner);
    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
}

#[test]
fn superseded_queued_value_drops_outside_scheduler_lock_and_may_reenter_batch() {
    let _tick = TEST_TICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let calls = Arc::new(AtomicUsize::new(0));
    let temporary_weak = Arc::new(Mutex::new(None));
    let probe = Cell::new(0_u64);
    let target = Cell::new(ReentrantValue::plain(0));
    let received = Arc::new(Mutex::new(Vec::new()));
    let received_by_callback = received.clone();
    let guard = target.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            received_by_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(value.value);
        }
    });
    received
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();

    let cleanup = Arc::new(ReentrantCleanup {
        calls: calls.clone(),
        temporary_weak: temporary_weak.clone(),
        probe: probe.clone(),
    });
    batch(|| {
        target.set(ReentrantValue {
            value: 1,
            cleanup: Some(cleanup),
        });
        target.set(ReentrantValue::plain(2));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "superseded capture was not dropped before enqueue returned"
        );
    });

    let temporary_is_dead = temporary_weak
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(|weak| !weak.is_alive());
    assert!(
        temporary_is_dead,
        "the nested queued closure retained its Cell after the outer drain"
    );
    assert_eq!(probe.get(), 99);
    assert_eq!(target.get().value, 2);
    assert_eq!(
        *received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![2]
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(guard);
}

#[test]
fn no_coalesce_scope_stamps_only_cells_born_inside_it_and_keeps_every_value() {
    let _tick = TEST_TICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ordinary = Cell::new(0_u64);
    let event = no_coalesce(|| Cell::new(0_u64));
    let factory = no_coalesce(|| || Cell::new(0_u64));
    let built_later = factory();

    assert!(!DepNode::no_coalesce(&ordinary));
    assert!(DepNode::no_coalesce(&event));
    assert!(!DepNode::no_coalesce(&built_later));

    let received = Arc::new(Mutex::new(Vec::new()));
    let received_by_callback = received.clone();
    let guard = event.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            let mut values = received_by_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            values.push(**value);
        }
    });
    received
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();

    batch(|| {
        for value in 1_u64..=8 {
            event.set(value);
        }
    });

    assert_eq!(
        *received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        (1_u64..=8).collect::<Vec<_>>()
    );
    drop(guard);
}
