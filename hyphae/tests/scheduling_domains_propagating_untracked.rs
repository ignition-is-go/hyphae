//! `Domain::propagating` before and after the first child domain exists.
//!
//! This file holds one test, so no other test in its process creates a child
//! domain first.

use std::sync::Arc;

use hyphae::{Cell, Mutable, Watchable, scheduler::Domain};
use parking_lot::Mutex;

#[test]
fn propagations_are_tracked_once_the_first_child_domain_exists() {
    let seen: Arc<Mutex<Vec<Option<Domain>>>> = Arc::default();
    let cell = Cell::new(0i64);
    let _guard = {
        let seen = Arc::clone(&seen);
        cell.subscribe(move |_| seen.lock().push(Domain::propagating()))
    };

    cell.set(1);
    let _child = Domain::child();
    cell.set(2);

    assert_eq!(
        *seen.lock(),
        [None, None, Some(Domain::root())],
        "the subscription, a root cascade before any child, and one after"
    );
}
