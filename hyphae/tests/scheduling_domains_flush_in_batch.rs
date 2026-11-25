//! `Domain::flush` called inside the caller's own open `batch`.
//!
//! The flush drains work another thread queued. That work's downstream
//! propagation belongs to the drained tick, not to the caller's batch, so it
//! is visible when `flush` returns even though the batch is still open.

use std::thread;

use hyphae::{Cell, Gettable, MapExt, Materialize, Mutable, batch, scheduler::Domain};

#[test]
fn flush_inside_open_batch_propagates_another_threads_write_downstream() {
    let source = Cell::new(0u64);
    let derived = source.clone().map(|value| value + 1).materialize();
    let child = Domain::child();

    batch(|| {
        let writer = source.clone();
        thread::spawn(move || writer.set(5)).join().unwrap();
        child.flush();
        assert_eq!(
            derived.get(),
            6,
            "a write drained by flush reaches its derived cell before the open batch closes"
        );
    });

    assert_eq!(derived.get(), 6);
}
