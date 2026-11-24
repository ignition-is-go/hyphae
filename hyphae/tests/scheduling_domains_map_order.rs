//! A map's diffs reach its subscribers in the order its rows were written,
//! whichever thread holds a batch open.

use std::{sync::Arc, thread};

mod domain_support;

use domain_support::{Gate, ROOT_TICK_SERIAL};
use hyphae::{CellMap, MapQuery, SelectExt, batch, scheduler::Domain};

/// One thread writes a key inside a batch it keeps open. Another thread then
/// writes the same key. The map holds the second write, so a map derived from
/// it must hold the second write too once both have propagated.
fn assert_a_derived_map_follows_the_later_write(domain: &Domain) {
    let (source, derived) = domain.enter(|| {
        let source = CellMap::<u32, i64>::new();
        let derived = source.clone().select(|_| true).materialize();
        (source, derived)
    });
    let gate = Arc::new(Gate::default());
    gate.arm();

    let earlier = {
        let (domain, source, gate) = (domain.clone(), source.clone(), Arc::clone(&gate));
        thread::spawn(move || {
            domain.batch(|| {
                source.insert(1, 10);
                gate.pass();
            });
        })
    };
    assert!(
        gate.wait_entered(),
        "the earlier writer never opened its batch"
    );

    domain.batch(|| drop(source.insert(1, 20)));

    gate.open();
    assert!(earlier.join().is_ok());
    domain.flush();
    assert_eq!(source.get_value(&1), Some(20));
    assert_eq!(
        derived.get_value(&1),
        source.get_value(&1),
        "the derived map holds the earlier write and the source holds the later one"
    );
}

#[test]
fn a_root_map_write_staged_in_an_open_batch_does_not_arrive_after_a_later_write() {
    let _serial = ROOT_TICK_SERIAL.lock();
    assert_a_derived_map_follows_the_later_write(&Domain::root());
}

#[test]
fn a_child_map_write_staged_in_an_open_batch_does_not_arrive_after_a_later_write() {
    let _serial = ROOT_TICK_SERIAL.lock();
    assert_a_derived_map_follows_the_later_write(&Domain::child());
}
