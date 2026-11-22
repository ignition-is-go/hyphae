//! Observable `switch_map` contract at the materialized cell: what a
//! subscriber sees on switch, on stale-inner writes, under `batch`, under
//! `no_coalesce`, and on teardown.

use std::sync::Arc;

use hyphae::{
    Cell, DepNode, Gettable, JoinExt, MapExt, Materialize, Mutable, Signal, SwitchMapExt,
    Watchable, batch, scheduler::no_coalesce,
};
use parking_lot::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> parking_lot::MutexGuard<'static, ()> {
    hyphae::scheduler::set_wave_threshold_for_test(4);
    SERIAL.lock()
}

fn record<T: Clone + Send + Sync + 'static>(
    cell: &impl Watchable<T>,
) -> (Arc<Mutex<Vec<T>>>, hyphae::SubscriptionGuard) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let guard = cell.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            sink.lock().push((**v).clone());
        }
    });
    (seen, guard)
}

type Inner = Cell<u64, hyphae::CellMutable>;

fn selector() -> (Cell<usize, hyphae::CellMutable>, [Inner; 2]) {
    (Cell::new(0usize), [Cell::new(10u64), Cell::new(20u64)])
}

fn pick([first, second]: [Inner; 2]) -> impl Fn(&usize) -> Inner + Send + Sync + 'static {
    move |&i| {
        if i == 0 {
            first.clone()
        } else {
            second.clone()
        }
    }
}

#[test]
fn switch_delivers_the_new_inner_current_value_immediately() {
    let _serial = serial();
    let (sel, inners) = selector();
    let out = sel.clone().switch_map(pick(inners)).materialize();
    let (seen, _g) = record(&out);
    sel.set(1);
    assert_eq!(out.get(), 20);
    batch(|| sel.set(0));
    assert_eq!(out.get(), 10);
    assert_eq!(*seen.lock(), vec![10, 20, 10]);
}

#[test]
fn a_stale_inner_is_never_delivered_after_a_switch() {
    let _serial = serial();
    let (sel, inners) = selector();
    let out = sel
        .clone()
        .switch_map(pick(inners.clone()))
        .map(|v| v + 1)
        .materialize();
    let (seen, _g) = record(&out);
    sel.set(1);
    inners[0].set(11);
    batch(|| {
        sel.set(0);
        inners[1].set(21);
    });
    inners[1].set(22);
    assert_eq!(out.get(), 12);
    assert_eq!(*seen.lock(), vec![11, 21, 12]);
}

#[test]
fn switch_output_depends_directly_on_outer_and_current_inner() {
    let _serial = serial();
    let (sel, inners) = selector();
    let out = sel.clone().switch_map(pick(inners.clone())).materialize();
    let deps = |c: &dyn DepNode| {
        let mut ids: Vec<_> = c.deps().iter().map(|d| d.id()).collect();
        ids.sort();
        ids
    };
    let mut want = vec![sel.id(), inners[0].id()];
    want.sort();
    assert_eq!(deps(&out), want);
    sel.set(1);
    let mut want = vec![sel.id(), inners[1].id()];
    want.sort();
    assert_eq!(deps(&out), want);
}

#[test]
fn batched_diamond_through_a_switch_is_glitch_free() {
    let _serial = serial();
    let sel = Cell::new(0u64);
    let doubled = sel.clone().map(|v| v * 2).materialize();
    let out = sel
        .clone()
        .switch_map(move |&v| Cell::new(v * 2))
        .materialize();
    let sink = out.join(doubled).materialize();
    let (seen, _g) = record(&sink);
    batch(|| sel.set(1));
    batch(|| sel.set(2));
    assert_eq!(*seen.lock(), vec![(0, 0), (2, 2), (4, 4)]);
}

#[test]
fn dropping_the_output_unsubscribes_outer_and_inner() {
    let _serial = serial();
    let (sel, inners) = selector();
    let out = sel
        .clone()
        .switch_map(pick(inners.clone()))
        .map(|v| v * 3)
        .materialize();
    assert_eq!(sel.subscriber_count(), 1);
    assert_eq!(inners[0].subscriber_count(), 1);
    sel.set(1);
    assert_eq!(inners[0].subscriber_count(), 0);
    assert_eq!(inners[1].subscriber_count(), 1);
    drop(out);
    assert_eq!(sel.subscriber_count(), 0);
    assert_eq!(inners[1].subscriber_count(), 0);
}

#[test]
fn no_coalesce_events_pass_a_switch_each_in_order() {
    let _serial = serial();
    let (sel, inner, out) = no_coalesce(|| {
        let sel = Cell::new(0u64);
        let inner = Cell::new(0u64);
        let i = inner.clone();
        let out = sel.clone().switch_map(move |_| i.clone()).materialize();
        (sel, inner, out)
    });
    let (seen, _g) = record(&out);
    batch(|| {
        inner.set(1);
        inner.set(2);
        inner.set(3);
    });
    let _ = sel;
    assert_eq!(*seen.lock(), vec![0, 1, 2, 3]);
}

#[test]
fn equal_inner_values_are_deduplicated() {
    let _serial = serial();
    let sel = Cell::new(0u64);
    let inner = Cell::new(5u64);
    let i = inner.clone();
    let out = sel.clone().switch_map(move |_| i.clone()).materialize();
    let (seen, _g) = record(&out);
    // Cells drop a value equal to the one they hold: `inner` drops the
    // repeated 5, and the output drops the 5 the switch re-delivers.
    inner.set(5);
    sel.set(1);
    assert_eq!(*seen.lock(), vec![5]);
}

#[test]
fn names_attach_to_the_materialized_output() {
    let _serial = serial();
    let sel = Cell::new(0u64);
    let out = sel
        .switch_map(|&v| Cell::new(v))
        .materialize()
        .with_name("switched");
    assert_eq!(out.name().as_deref(), Some("switched"));
}

#[test]
fn completion_waits_for_outer_and_current_inner() {
    let _serial = serial();
    let sel = Cell::new(0u64);
    let inner = Cell::new(1u64);
    let i = inner.clone();
    let out = sel.clone().switch_map(move |_| i.clone()).materialize();
    sel.complete();
    assert!(!out.is_complete());
    inner.complete();
    assert!(out.is_complete());
}
