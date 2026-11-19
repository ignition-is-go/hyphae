use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use parking_lot::Mutex;

use crate::{
    Cell, CellImmutable, CellMap, Gettable, KeyedFamily, LookupExt, LookupSetExt, MapDiff, MapExt,
    Materialize, Mutable, Signal, Watchable, batch, family, flush,
};

type Rows = KeyedFamily<u32, Cell<i32, CellImmutable>>;

/// Rows over one store cell: row k is `10 * store + k`. Counts builds.
fn store_family(store: &Cell<i32, crate::CellMutable>) -> (Rows, Arc<AtomicUsize>) {
    let builds = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&builds);
    let store = store.clone();
    let rows = family(move |k: &u32| {
        counted.fetch_add(1, Ordering::SeqCst);
        let k = i32::from(u8::try_from(*k).unwrap_or(u8::MAX));
        store
            .clone()
            .map(move |s| s.saturating_mul(10).saturating_add(k))
            .materialize()
    });
    (rows, builds)
}

fn record<T: crate::CellValue>(cell: &Cell<T, CellImmutable>) -> Arc<Mutex<Vec<T>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let guard = cell.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            sink.lock().push(T::clone(value));
        }
    });
    let _ = guard.leak();
    seen
}

#[test]
fn key_change_reads_the_new_row() {
    let store = Cell::new(1);
    let (rows, _) = store_family(&store);
    let key = Cell::new(0u32);
    let out = key.clone().lookup(rows);
    assert_eq!(out.get(), 10);

    key.set(2);
    assert_eq!(out.get(), 12);
    store.set(3);
    assert_eq!(out.get(), 32);
}

#[test]
fn old_row_changes_do_not_reach_the_reader() {
    let a = Cell::new(1);
    let b = Cell::new(100);
    let (ra, rb) = (a.clone(), b.clone());
    let rows = family(move |k: &usize| if *k == 0 { ra.clone() } else { rb.clone() }.lock());
    let key = Cell::new(0usize);
    let out = key.clone().lookup(rows);
    let seen = record(&out);

    key.set(1);
    a.set(2);
    b.set(101);
    assert_eq!(*seen.lock(), vec![1, 100, 101]);
}

#[test]
fn key_and_entry_in_one_batch_emit_once_with_both() {
    let store = Cell::new(1);
    let (rows, _) = store_family(&store);
    let key = Cell::new(0u32);
    let out = key.clone().lookup(rows);
    let seen = record(&out);

    batch(|| {
        key.set(1);
        store.set(5);
    });
    assert_eq!(*seen.lock(), vec![10, 51]);
}

#[test]
fn key_and_entry_in_adjacent_writes_emit_each_state() {
    let store = Cell::new(1);
    let (rows, _) = store_family(&store);
    let key = Cell::new(0u32);
    let out = key.clone().lookup(rows);
    let seen = record(&out);

    key.set(1);
    store.set(5);
    store.set(6);
    key.set(0);
    assert_eq!(*seen.lock(), vec![10, 11, 51, 61, 60]);
}

#[test]
fn entry_then_key_in_one_batch_reads_the_entry() {
    let store = Cell::new(1);
    let (rows, _) = store_family(&store);
    let key = Cell::new(0u32);
    let out = key.clone().lookup(rows);
    let seen = record(&out);

    batch(|| {
        store.set(7);
        key.set(3);
    });
    assert_eq!(*seen.lock(), vec![10, 73]);
}

#[test]
fn rows_are_built_on_first_demand_and_shared() {
    let store = Cell::new(1);
    let (rows, builds) = store_family(&store);
    assert!(rows.is_empty());

    let key = Cell::new(4u32);
    let first = key.clone().lookup(rows.clone());
    let second = key.lookup(rows.clone());
    flush();
    assert_eq!((first.get(), second.get()), (14, 14));
    assert_eq!(rows.len(), 1);
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}

#[test]
fn the_family_tears_down_an_undemanded_row_and_rebuilds_it() {
    let store = Cell::new(1);
    let (rows, builds) = store_family(&store);
    let key = Cell::new(0u32);
    let out = key.clone().lookup(rows.clone());
    key.set(1);
    flush();
    assert!(!rows.contains(&0));
    assert!(rows.contains(&1));

    drop(out);
    flush();
    assert!(rows.is_empty());

    store.set(9);
    let again = key.lookup(rows);
    assert_eq!(again.get(), 91);
    assert_eq!(builds.load(Ordering::SeqCst), 3);
}

#[test]
fn a_row_released_and_demanded_in_one_batch_is_kept() {
    let store = Cell::new(1);
    let (rows, builds) = store_family(&store);
    let left = Cell::new(0u32);
    let right = Cell::new(1u32);
    let a = left.clone().lookup(rows.clone());
    let b = right.clone().lookup(rows);
    flush();
    assert_eq!(builds.load(Ordering::SeqCst), 2);

    batch(|| {
        left.set(1);
        right.set(0);
    });
    flush();
    assert_eq!((a.get(), b.get()), (11, 10));
    assert_eq!(builds.load(Ordering::SeqCst), 2);
}

#[test]
fn none_key_reads_none_and_holds_no_row() {
    let store = Cell::new(1);
    let (rows, _) = store_family(&store);
    let key = Cell::new(None::<u32>);
    let out = key.clone().lookup(rows.clone());
    let seen = record(&out);
    assert_eq!(out.get(), None);

    key.set(Some(2));
    store.set(3);
    key.set(None);
    store.set(4);
    flush();
    assert_eq!(*seen.lock(), vec![None, Some(12), Some(32), None]);
    assert!(rows.is_empty());
}

/// Replays a diff stream the way a subscriber holds it. An insert must name
/// an absent key and a remove or update a present one.
fn hold(held: &mut BTreeMap<u32, i32>, diff: &MapDiff<u32, i32>) -> Result<(), String> {
    match diff {
        MapDiff::Initial { entries } => *held = entries.iter().copied().collect(),
        MapDiff::Insert { key, value } => {
            if held.insert(*key, *value).is_some() {
                return Err(format!("insert of held key {key}"));
            }
        }
        MapDiff::Remove { key, .. } => {
            if held.remove(key).is_none() {
                return Err(format!("remove of absent key {key}"));
            }
        }
        MapDiff::Update { key, new_value, .. } => {
            if held.insert(*key, *new_value).is_none() {
                return Err(format!("update of absent key {key}"));
            }
        }
        MapDiff::Batch { changes } => {
            for change in changes {
                hold(held, change)?;
            }
        }
    }
    Ok(())
}

struct Told {
    diffs: Arc<Mutex<Vec<MapDiff<u32, i32>>>>,
    held: Arc<Mutex<BTreeMap<u32, i32>>>,
    errors: Arc<Mutex<Vec<String>>>,
}

impl Told {
    /// What the subscriber holds, after checking every diff applied to it.
    fn held(&self) -> BTreeMap<u32, i32> {
        assert_eq!(*self.errors.lock(), Vec::<String>::new());
        self.held.lock().clone()
    }
}

fn tell(view: &CellMap<u32, i32, CellImmutable>) -> Told {
    let told = Told {
        diffs: Arc::default(),
        held: Arc::default(),
        errors: Arc::default(),
    };
    let (d, h, e) = (
        Arc::clone(&told.diffs),
        Arc::clone(&told.held),
        Arc::clone(&told.errors),
    );
    let _ = view
        .subscribe_diffs(move |diff| {
            let held = hold(&mut h.lock(), diff);
            if let Err(error) = held {
                e.lock().push(error);
            }
            d.lock().push(diff.clone());
        })
        .leak();
    told
}

type Tables = (CellMap<u32, i32>, CellMap<u32, i32>);

/// A view family over two fixed tables: row 0 holds `0 -> 0` and row 1 holds
/// `1 -> 1`.
fn views() -> (KeyedFamily<u32, CellMap<u32, i32>>, Tables) {
    let tables = (
        CellMap::from_entries(vec![(0, 0)]),
        CellMap::from_entries(vec![(1, 1)]),
    );
    let rows = tables.clone();
    let family = family(move |k: &u32| {
        if *k == 0 {
            rows.0.clone()
        } else {
            rows.1.clone()
        }
    });
    (family, tables)
}

#[test]
fn view_switch_tells_one_initial_then_new_rows_only() {
    let (rows, tables) = views();
    let key = Cell::new(0u32);
    let view = key.clone().lookup(rows);
    let told = tell(&view);
    told.diffs.lock().clear();

    key.set(1);
    assert_eq!(
        *told.diffs.lock(),
        vec![MapDiff::Initial {
            entries: vec![(1, 1)]
        }]
    );
    tables.0.insert(7, 7);
    tables.1.insert(8, 8);
    assert_eq!(told.held(), BTreeMap::from([(1, 1), (8, 8)]));
    assert_eq!(told.diffs.lock().len(), 2);
}

#[test]
fn view_key_and_row_in_one_batch_tell_the_row_once() {
    let (rows, tables) = views();
    let key = Cell::new(0u32);
    let view = key.clone().lookup(rows);
    let told = tell(&view);

    batch(|| {
        key.set(1);
        tables.1.insert(5, 5);
    });
    tables.1.insert(6, 6);
    assert_eq!(told.held(), BTreeMap::from([(1, 1), (5, 5), (6, 6)]));
    assert_eq!(view.snapshot().len(), 3);
}

#[test]
fn view_row_then_key_in_adjacent_writes_tell_the_snapshot() {
    let (rows, tables) = views();
    let key = Cell::new(0u32);
    let view = key.clone().lookup(rows);
    let told = tell(&view);

    tables.1.insert(5, 5);
    key.set(1);
    tables.1.remove(&1);
    assert_eq!(told.held(), BTreeMap::from([(5, 5)]));
}

#[test]
fn view_none_key_holds_no_entries() {
    let (rows, _) = views();
    let key = Cell::new(Some(0u32));
    let view = key.clone().lookup(rows);
    let told = tell(&view);
    key.set(None);
    assert!(told.held().is_empty());
    key.set(Some(1));
    assert_eq!(told.held(), BTreeMap::from([(1, 1)]));
}

#[test]
fn set_join_follows_keys_and_rows() {
    let store = Cell::new(1);
    let (rows, _) = store_family(&store);
    let keys = Cell::new(BTreeSet::from([0u32]));
    let joined = keys.clone().lookup_set(rows.clone());
    let seen = record(&joined);
    assert_eq!(joined.get(), BTreeMap::from([(0, 10)]));

    keys.set(BTreeSet::from([0, 1]));
    assert_eq!(joined.get(), BTreeMap::from([(0, 10), (1, 11)]));

    batch(|| {
        store.set(2);
        keys.set(BTreeSet::from([1, 2]));
    });
    assert_eq!(joined.get(), BTreeMap::from([(1, 21), (2, 22)]));
    flush();
    assert!(!rows.contains(&0));

    store.set(3);
    assert_eq!(joined.get(), BTreeMap::from([(1, 31), (2, 32)]));
    let seen = seen.lock().clone();
    assert_eq!(seen.last(), Some(&BTreeMap::from([(1, 31), (2, 32)])));
    assert!(
        !seen.contains(&BTreeMap::from([(1, 11), (2, 22)])),
        "a batch's key and row changes are told together: {seen:?}"
    );
}
