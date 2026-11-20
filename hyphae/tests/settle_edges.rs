//! The debug edge check: a callback write that joins the settle must travel a
//! declared edge, unless it is marked as a new ticket.
#![cfg(debug_assertions)]

use std::panic::{AssertUnwindSafe, catch_unwind};

use hyphae::{Cell, Gettable, MapExt, Materialize, Mutable, Watchable, settle};

/// A cell above height 0 that `trigger`'s callbacks can write.
fn raised_target() -> (
    Cell<i32, hyphae::CellMutable>,
    Cell<i32, hyphae::CellMutable>,
) {
    let below = Cell::new(0);
    let target = Cell::new(0);
    target.own(below.subscribe(|_| {}));
    (below, target)
}

#[test]
fn undeclared_callback_write_trips_and_marked_one_does_not() {
    let (_below, target) = raised_target();
    let trigger = Cell::new(0);
    let _guard = trigger.subscribe(move |signal| {
        if let hyphae::Signal::Value(value) = signal {
            target.set(**value);
        }
    });
    let tripped = catch_unwind(AssertUnwindSafe(|| trigger.set(1)));
    assert!(tripped.is_err(), "an undeclared joined write panics");

    let (_below, target) = raised_target();
    let trigger = Cell::new(0);
    let written = target.clone();
    let _guard = trigger.subscribe(move |signal| {
        if let hyphae::Signal::Value(value) = signal {
            let value = **value;
            settle::new_ticket(|| written.set(value));
        }
    });
    trigger.set(2);
    assert_eq!(target.get(), 2);

    let source = Cell::new(1);
    let derived = source.clone().map(|value| value + 1).materialize();
    source.set(5);
    assert_eq!(
        derived.get(),
        6,
        "an operator write travels its declared edge"
    );
}
