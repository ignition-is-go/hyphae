//! Regression: `CellMap::size()`/`len()` must retain its parent map, like
//! `entries()`/`items()`/`subscribe_diffs`. Before the keepalive fix, `.size()`
//! returned a bare clone of the internal `len_cell` with no reference back to
//! the `CellMapInner`, so a `.size()` cloned out of a temporary map (e.g.
//! `query.materialize().size()`, or myko's `query_map_by_str(q).size()`) let the
//! map — and the source subscription feeding `len_cell` — drop at the end of the
//! statement, silently freezing the count. This surfaced as CountAll cells
//! stuck at 0 on canary.71.
#![cfg(feature = "scheduler")]

use hyphae::{CellMap, Gettable, MapQuery, SelectExt, batch};

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

fn populated(n: i32) -> CellMap<String, i32> {
    let store = CellMap::<String, i32>::new();
    for i in 0..n {
        store.insert(format!("k{i}"), i);
    }
    store
}

/// The bug's core: a count whose intermediate materialized map is dropped
/// (only the `.len()` cell is kept) must still track later mutations — the
/// returned cell has to keep the map's source subscription alive.
#[test]
fn count_tracks_after_intermediate_map_dropped() {
    let _s = serial();
    let store = CellMap::<String, i32>::new();
    // `.materialize()` temporary drops at the end of this statement; only `count`
    // survives. Pre-fix this severed the store subscription.
    let count = store.clone().select(|_| true).materialize().len();
    assert_eq!(count.get(), 0);
    for i in 0..5 {
        store.insert(format!("k{i}"), i);
    }
    assert_eq!(
        count.get(),
        5,
        "count froze after the intermediate map dropped"
    );
}

/// Fresh count over an already-populated store reads N (both eager and when the
/// knit is deferred through a batch drain).
#[test]
fn fresh_count_on_populated_store_reads_n() {
    let _s = serial();
    let store = populated(5);
    let eager = store.clone().select(|_| true).materialize().len();
    assert_eq!(eager.get(), 5, "fresh count (eager) read wrong");

    let store2 = populated(5);
    let batched = batch(|| store2.clone().select(|_| true).materialize().len());
    assert_eq!(
        batched.get(),
        5,
        "fresh count (knit under batch) read wrong"
    );
}

/// Holding the materialized map alive always worked; keep it as a control so a
/// future change that breaks the held-alive path is caught too.
#[test]
fn count_tracks_with_materialized_held() {
    let _s = serial();
    let store = CellMap::<String, i32>::new();
    let materialized = store.clone().select(|_| true).materialize();
    let count = materialized.len();
    for i in 0..5 {
        store.insert(format!("k{i}"), i);
    }
    assert_eq!(count.get(), 5);
}

/// `size()` on a base map (no select layer) also retains correctly after the
/// handle it was taken from goes out of scope.
#[test]
fn base_map_size_tracks_after_handle_scope() {
    let _s = serial();
    let store = populated(3);
    let size = { store.clone().size() };
    assert_eq!(size.get(), 3);
    store.insert("k99".to_string(), 99);
    assert_eq!(size.get(), 4);
}
