//! A family reader's quiet certificate survives writes its row never reads.
//!
//! Every family anchors to a root that hears every store. A write to store
//! T seeds the root, which feeds the family node, but no edge a seed clears
//! leads from the family node to a reader: the reader hears the family only
//! through an edge gated by its key. So a join that reads the reader as a
//! quiet input keeps its certificate across T's writes, and validations do
//! not grow with them.

use hyphae::{
    Cell, CellMap, Gettable, JoinExt, LookupExt, MapDiff, Materialize, Mutable, Signal, Watchable,
    family, flush, settle,
};

#[test]
fn writes_a_row_never_reads_leave_its_reader_quiet() {
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
    let before = settle::stats().validated;
    for round in 0..rounds {
        active.set(round);
        t.insert(round, round);
    }
    flush();
    let validated = settle::stats().validated - before;
    assert_eq!(joined.get(), (Some(10), rounds - 1));
    assert!(
        validated < u64::from(rounds) / 4,
        "{validated} validations over {rounds} writes the row never reads"
    );
}
