//! Regression guard for a reported switch_map retention leak: an inner cell
//! built while a `batch()` drain is active must be reclaimed just like one built
//! synchronously. Mirrors the myko CuePaused shape: inner =
//! CellMap.items().map().materialize() (stand-in for query_map().items()...),
//! the switch driven by the same store the inner reads, wrapped in an outer
//! report-level materialize(). A bare-hyphae repro of every structural condition
//! reclaims cleanly — pinning that here so a future scheduler/switch_map change
//! that DID strand a strong clone would fail loudly.
#![cfg(feature = "scheduler")]

use hyphae::{
    Cell, CellImmutable, CellMap, Gettable, MapExt, MaterializeDefinite, SwitchMapExt, batch,
    cell::WeakCell,
};

fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

#[test]
fn switch_map_inner_reclaimed_when_built_under_batch_drain() {
    let _serial = scheduler_test_serial();

    // A persistent query source (like myko's store).
    let store: CellMap<String, i64> = CellMap::new();
    store.insert("a".to_string(), 1);

    // Weak refs to every inner materialized cell the factory builds.
    let weaks: std::sync::Arc<std::sync::Mutex<Vec<WeakCell<i64, CellImmutable>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    // The switch_map's OUTER is itself store-derived, so one store mutation both
    // re-fires the outer (triggering a switch) AND is mid-diff in the same drain
    // when the new inner subscribes to the same store.
    let selector = store.items().map(|vs| vs.len() as u64).materialize();

    let collector = weaks.clone();
    let store_for_factory = store.clone();
    let switched = selector.switch_map(move |k| {
        let k = *k;
        // Fresh inner chain per switch, subscribing to the SAME store whose diff
        // triggered this switch — built under the active drain for k >= 2.
        let inner = store_for_factory
            .items()
            .map(move |vs| vs.iter().sum::<i64>() + k as i64)
            .materialize();
        collector.lock().unwrap().push(inner.downgrade());
        inner
    });
    // Outer report-level materialize wrapping the switch_map output.
    let report = switched.map(|x| *x).materialize();
    let _ = report.get();

    // Each round: mutate the store inside a batch(); the diff fires the outer
    // (len changes → switch) while still propagating, so the new inner is built
    // under the active drain.
    for i in 1..=5u64 {
        batch(|| {
            store.insert(format!("k{i}"), i as i64);
        });
    }
    let _ = report.get();

    let built = weaks.lock().unwrap().len();
    let alive: Vec<usize> = weaks
        .lock()
        .unwrap()
        .iter()
        .enumerate()
        .filter_map(|(i, w)| w.upgrade().map(|_| i))
        .collect();

    // Only the current inner should remain alive; every superseded inner built
    // under the drain must have been reclaimed.
    assert!(
        alive.len() <= 1,
        "built {built} inners, expected <=1 live, found {} (indices {alive:?}) — old inners leaking",
        alive.len(),
    );
}
