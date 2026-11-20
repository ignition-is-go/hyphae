//! Building many nodes under one input never lays the reach index out
//! again. Each new dependent takes a place in the order next to its input,
//! and a dense spot is relabelled locally.

use hyphae::{Cell, Gettable, MapExt, Materialize, Mutable, settle};

#[test]
fn many_dependents_of_one_input_need_no_compaction() {
    settle::set_workers(0);
    let store = Cell::new(0u32);
    let before = settle::stats();
    let rows: Vec<_> = (0..5_000u32).map(|i| store.clone().map(move |v| v + i).materialize()).collect();
    store.set(1);
    let after = settle::stats();
    assert_eq!(rows.last().map(Gettable::get), Some(5_000));
    assert_eq!(
        after.index_rebuilds - before.index_rebuilds,
        0,
        "{} compactions while building dependents of one input",
        after.index_rebuilds - before.index_rebuilds
    );
}
