//! A family reader joins correctly across writes its row never reads.
//!
//! Every family anchors to a root that hears every store. A write to store
//! T reaches the family node, but the reader hears the family only through
//! an edge gated by its key, so those writes never hold the join back.

use hyphae::{
    Cell, CellMap, Gettable, JoinExt, LookupExt, MapDiff, Materialize, Mutable, Signal,
    family, flush, settle,
};

#[test]
fn writes_a_row_never_reads_leave_its_reader_joined() {
    settle::set_workers(0);
    let s = CellMap::<u32, u32>::new();
    s.insert(1, 10);
    let t = CellMap::<u32, u32>::new();
    let root = Cell::new(());
    for store in [&s, &t] {
        let fire = root.downgrade();
        root.own(store.subscribe_diffs(move |diff| {
            if !matches!(diff, MapDiff::Initial { .. })
                && let Some(root) = fire.upgrade()
            {
                root.notify(Signal::value(()));
            }
        }));
    }
    let rows = {
        let s = s.clone();
        family(move |id: &u32| s.get(id).materialize())
    };
    rows.anchor(&root);
    let key = Cell::new(1u32);
    let reader = key.clone().lookup(rows);
    let active = Cell::new(0u32);
    let joined = reader.join(active.clone()).materialize();

    let rounds = 200u32;
    for round in 0..20 {
        active.set(round);
        t.insert(round, round);
    }
    flush();
    for round in 0..rounds {
        active.set(round);
        t.insert(round, round);
    }
    flush();
    assert_eq!(joined.get(), (Some(10), rounds - 1));
}
