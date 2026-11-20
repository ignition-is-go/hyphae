//! A repeated result in a cell does not propagate. These tests pin what that
//! means at a join: when some parents stay silent because their value did
//! not change, the join still computes exactly once per write, on inputs that
//! all reflect the same write, and it never waits for a silent parent.
//!
//! Each diamond has one branch that passes the source on and other branches
//! that carry `source / 4`, which repeats three writes in four. The cell
//! below the join counts its evaluations and checks every branch against
//! branch 0.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use hyphae::{
    Cell, CellImmutable, CellMutable, Gettable, JoinExt, MapExt, Materialize, Mutable, Signal,
    SubscriptionGuard, Watchable, batch, join_vec,
};

/// The scheduler's queue is process-wide: while a batch is open on one
/// thread, a write on another is deferred and its batch can return before the
/// write is applied. Tests that assert right after a write take turns.
static TURN: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

struct Diamonds {
    sources: Vec<Cell<u64, CellMutable>>,
    evals: Arc<AtomicU64>,
    mixed: Arc<AtomicU64>,
    top: Cell<Vec<u64>, CellImmutable>,
}

/// `width` branches shared evenly between `writers` sources and joined once,
/// then `nest` more two-branch diamonds stacked on the join's output.
fn diamonds(width: usize, writers: usize, nest: usize) -> Diamonds {
    let sources: Vec<_> = (0..writers).map(|_| Cell::new(0u64)).collect();
    let evals = Arc::new(AtomicU64::new(0));
    let mixed = Arc::new(AtomicU64::new(0));
    let per = width / writers;
    let branches: Vec<_> = (0..width)
        .map(|i| {
            let source = sources[i / per].clone();
            if i % per == 0 {
                source.map(|v| *v).materialize()
            } else {
                source.map(|v| v / 4).materialize()
            }
        })
        .collect();
    let (count, bad) = (evals.clone(), mixed.clone());
    let mut top = join_vec(branches)
        .map(move |values| {
            count.fetch_add(1, Ordering::SeqCst);
            values
                .chunks(per)
                .map(|group| {
                    if group[1..].iter().any(|branch| *branch != group[0] / 4) {
                        bad.fetch_add(1, Ordering::SeqCst);
                    }
                    group[0]
                })
                .collect::<Vec<u64>>()
        })
        .materialize();
    for _ in 0..nest {
        let left = top.clone().map(Clone::clone).materialize();
        let right = top
            .clone()
            .map(|values| values.iter().map(|v| v / 4).collect::<Vec<u64>>())
            .materialize();
        let (count, bad) = (evals.clone(), mixed.clone());
        top = left
            .join(right)
            .map(move |(whole, quarters)| {
                count.fetch_add(1, Ordering::SeqCst);
                if whole.iter().zip(quarters).any(|(a, b)| *b != a / 4) {
                    bad.fetch_add(1, Ordering::SeqCst);
                }
                whole.clone()
            })
            .materialize();
    }
    Diamonds {
        sources,
        evals,
        mixed,
        top,
    }
}

const WRITES: u64 = 200;

/// Write `next(v)` [`WRITES`] times, one batch each, and return how many
/// times the joins computed in all.
fn join_evals(d: &Diamonds, v: &mut u64, next: impl Fn(u64) -> u64) -> u64 {
    let before = d.evals.load(Ordering::SeqCst);
    for _ in 0..WRITES {
        *v = next(*v);
        batch(|| d.sources[0].set(*v));
    }
    d.evals.load(Ordering::SeqCst) - before
}

const fn one_parent_changes(v: u64) -> u64 {
    if v % 4 == 3 { v + 2 } else { v + 1 }
}

#[test]
fn a_join_computes_once_on_consistent_inputs_when_parents_stay_silent() {
    let _turn = TURN.lock();
    for (width, nest) in [(2, 0), (8, 0), (64, 0), (2, 3), (8, 3)] {
        let d = diamonds(width, 1, nest);
        let joins = 1 + nest as u64;
        let mut v = 0;
        let context = format!("width {width}, {nest} nested");

        // Once per join per write.
        let once = WRITES * joins;
        let fired = join_evals(&d, &mut v, one_parent_changes);
        assert_eq!(fired, once, "{context}: one parent changes");
        let fired = join_evals(&d, &mut v, |v| v + 4);
        assert_eq!(fired, once, "{context}: all parents change");
        // Every parent is silent, so nothing below computes.
        let fired = join_evals(&d, &mut v, |v| v);
        assert_eq!(fired, 0, "{context}: no parent changes");
        // And the join has not stalled waiting for them.
        let fired = join_evals(&d, &mut v, one_parent_changes);
        assert_eq!(fired, once, "{context}: after silence");

        assert_eq!(d.mixed.load(Ordering::SeqCst), 0, "{context}: mixed inputs");
        assert_eq!(d.top.get(), vec![v], "{context}: stalled");
    }
}

#[test]
fn concurrent_writers_never_show_a_join_mixed_inputs_or_stall_it() {
    let _turn = TURN.lock();
    for (width, writers) in [(8, 2), (64, 4)] {
        let d = diamonds(width, writers, 2);
        let writes = 5_000u64;
        std::thread::scope(|scope| {
            for source in &d.sources {
                scope.spawn(move || {
                    for v in 1..=writes {
                        batch(|| source.set(v));
                    }
                });
            }
        });
        // A batch that closes while another thread drains returns before its
        // write is applied, so let the last drain finish.
        batch(|| ());
        let context = format!("width {width}, {writers} writers");
        assert_eq!(d.mixed.load(Ordering::SeqCst), 0, "{context}: mixed inputs");
        assert_eq!(d.top.get(), vec![writes; writers], "{context}: stalled");
    }
}

fn counted<T: hyphae::CellValue, M: Send + Sync + 'static>(
    cell: &Cell<T, M>,
) -> (Arc<AtomicU64>, SubscriptionGuard) {
    let hits = Arc::new(AtomicU64::new(0));
    let seen = hits.clone();
    let guard = cell.subscribe(move |signal| {
        if matches!(signal, Signal::Value(_)) {
            seen.fetch_add(1, Ordering::SeqCst);
        }
    });
    (hits, guard)
}

#[test]
fn a_repeated_value_is_not_emitted_and_a_changed_one_is() {
    let _turn = TURN.lock();
    let source = Cell::new(1u64);
    let halved = source.clone().map(|v| v / 2).materialize();
    let (source_hits, _a) = counted(&source);
    let (halved_hits, _b) = counted(&halved);

    source.set(1);
    assert_eq!(
        source_hits.load(Ordering::SeqCst),
        1,
        "only the initial value"
    );
    source.set(2);
    source.set(3);
    assert_eq!(source_hits.load(Ordering::SeqCst), 3);
    // 2 / 2 and 3 / 2 are both 1: the derived cell emitted once for them.
    assert_eq!(halved_hits.load(Ordering::SeqCst), 2);
    assert_eq!(halved.get(), 1);
}

#[test]
fn a_cell_that_keeps_every_write_still_drops_a_repeated_one() {
    let _turn = TURN.lock();
    // `no_coalesce` keeps every write as its own operation. It does not keep
    // repeats: a pulse with the payload the cell holds is dropped.
    let pulse = hyphae::scheduler::no_coalesce(|| Cell::new(0u64));
    let (hits, _guard) = counted(&pulse);
    batch(|| {
        pulse.set(7);
        pulse.set(7);
        pulse.set(8);
    });
    assert_eq!(hits.load(Ordering::SeqCst), 3, "initial, 7 and 8");
}

#[test]
fn a_cell_of_a_single_value_type_is_a_trigger_and_always_emits() {
    let _turn = TURN.lock();
    let trigger = Cell::new(());
    let (hits, _guard) = counted(&trigger);
    trigger.set(());
    trigger.set(());
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

#[test]
fn completion_passes_a_cell_that_holds_the_same_value() {
    let _turn = TURN.lock();
    let source = Cell::new(1u64);
    source.set(1);
    source.complete();
    assert!(source.is_complete());
}
