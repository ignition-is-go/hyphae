//! Regression guard for a subscriber-snapshot retention leak on the scheduler
//! batch path (reported via the myko CuePaused `switch_map` pattern).
//!
//! `SubscriberRegistry::remove` only marks the registry dirty; its cached notify
//! `snapshot` is rebuilt lazily on the *next* notify. A cell that is never
//! notified again — a superseded `switch_map` inner's `CellMap` `diffs_cell` —
//! would keep the just-unsubscribed subscriber pinned in that stale snapshot
//! forever, and because `CellMap::items()`/`subscribe_diffs` closures hold a
//! strong `map_keepalive` `Arc<CellMapInner>`, the whole map (and its own
//! upstream maps) leaked. It only manifested under `batch()`, where the drain
//! ordering leaves the last snapshot rebuild *before* the teardown; the
//! synchronous path happened to notify once more and rebuild.
//!
//! Shape that reproduced it (all three needed): two stacked `CellMap` layers
//! (one wrapping another via `subscribe_diffs`), a weak-ref query cache, and a
//! `switch_map` replacing the cached value each round under `batch()`.
#![cfg(feature = "scheduler")]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use hyphae::{
    CellImmutable, CellMap, Gettable, MapExt, MaterializeDefinite, Signal, SwitchMapExt, Watchable,
    batch,
    cell_map::WeakCellMap,
};

fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Weak-ref query cache keyed by params — mirrors myko's query cache.
struct QueryCache {
    cache: Mutex<HashMap<String, WeakCellMap<Arc<str>, Arc<i64>>>>,
}

impl QueryCache {
    fn new() -> Self {
        Self { cache: Mutex::new(HashMap::new()) }
    }

    fn get_or_build(
        &self,
        key: &str,
        build: impl FnOnce() -> CellMap<Arc<str>, Arc<i64>, CellImmutable>,
    ) -> CellMap<Arc<str>, Arc<i64>, CellImmutable> {
        let mut cache = self.cache.lock().unwrap();
        if let Some(weak) = cache.get(key)
            && let Some(strong) = weak.upgrade()
        {
            return strong.lock();
        }
        let built = build();
        cache.insert(key.to_string(), built.downgrade());
        built
    }

    fn live_count(&self) -> usize {
        self.cache.lock().unwrap().values().filter(|w| w.upgrade().is_some()).count()
    }
}

// Layer 1: per-id subscriptions on the permanent store's per-key cells, weakly
// capturing the result map.
fn build_ids_source_map(
    store: &CellMap<Arc<str>, Arc<i64>, CellImmutable>,
    ids: &[Arc<str>],
) -> CellMap<Arc<str>, Arc<i64>, CellImmutable> {
    let result: CellMap<Arc<str>, Arc<i64>> = CellMap::new();
    for id in ids {
        let key_cell = store.get(id);
        let result_weak = result.downgrade();
        let key_for_cb = id.clone();
        let guard = key_cell.subscribe(move |signal| {
            let Some(result_for_cb) = result_weak.upgrade() else {
                return;
            };
            if let Signal::Value(v) = signal {
                match v.as_ref() {
                    Some(item) => {
                        result_for_cb.insert(key_for_cb.clone(), item.clone());
                    }
                    None => {
                        result_for_cb.remove(&key_for_cb);
                    }
                }
            }
        });
        result.own(guard);
    }
    result.lock()
}

// Layer 2: a second CellMap that subscribes to layer 1's diffs. Removing this
// layer (caching layer 1 directly) made the leak vanish — two stacked layers
// are required to reproduce.
fn typed_wrap(
    source: CellMap<Arc<str>, Arc<i64>, CellImmutable>,
) -> CellMap<Arc<str>, Arc<i64>, CellImmutable> {
    let typed: CellMap<Arc<str>, Arc<i64>> = CellMap::new();
    let weak = typed.downgrade();
    let guard = source.subscribe_diffs(move |diff| {
        let Some(typed) = weak.upgrade() else {
            return;
        };
        typed.apply_batch(vec![diff.clone()]);
    });
    typed.own_guard(guard);
    typed.lock()
}

#[test]
fn stacked_cellmap_query_cache_reclaims_under_batch() {
    let _serial = scheduler_test_serial();

    let store_mutable: CellMap<Arc<str>, Arc<i64>, hyphae::CellMutable> = CellMap::new();
    for i in 0..5 {
        store_mutable.insert(format!("beta-{i}").into(), Arc::new(i));
    }
    let store = store_mutable.clone().lock();

    let outer_items = store.clone().items();
    let store_for_closure = store.clone();
    let query_cache = Arc::new(QueryCache::new());
    let query_cache_for_closure = query_cache.clone();

    // switch_map builds a fresh 2-layer cached query per outer emission.
    let switched = outer_items.switch_map(move |items| {
        let ids: Vec<Arc<str>> =
            (0..items.len() as i64).map(|i| format!("id-{i}").into()).collect();
        let cache_key = format!("{ids:?}");
        let typed = query_cache_for_closure.get_or_build(&cache_key, || {
            typed_wrap(build_ids_source_map(&store_for_closure, &ids))
        });
        typed
            .items()
            .map(|vals| Arc::new(vals.iter().map(|v| **v).sum::<i64>()))
            .materialize()
    });

    let report_cell = switched.materialize();
    let _ = report_cell.get();

    // Each round mutates the store inside a batch(), which re-fires the outer
    // (map length changes) and switches to a fresh cached query — superseding
    // the previous one, which must be reclaimed.
    for i in 5..=30u64 {
        batch(|| {
            store_mutable.insert(format!("stress-{i}").into(), Arc::new(i as i64));
        });
        let _ = report_cell.get();
    }

    // Only the current query should be live while the report is held...
    assert!(
        query_cache.live_count() <= 1,
        "expected <=1 live cached query while report held, found {} — superseded queries leaking",
        query_cache.live_count()
    );

    // ...and none after the whole graph is dropped.
    drop(report_cell);
    assert_eq!(
        query_cache.live_count(),
        0,
        "expected 0 live cached queries after dropping the report, found {} — leak",
        query_cache.live_count()
    );
}
