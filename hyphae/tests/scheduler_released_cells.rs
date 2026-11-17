//! A cell nobody holds any more must stop recomputing, even while the tick
//! queue is busy. A queued op that keeps its cell alive makes an abandoned cell
//! immortal whenever its source is rewritten before that op runs: each rewrite
//! reaches the cell through the op's reference and queues a fresh op that holds
//! it again, so it never unsubscribes and its work never goes away.
#![cfg(feature = "scheduler")]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use hyphae::{Cell, MapExt, Materialize, Mutable, Signal, Watchable, batch};
use parking_lot::Mutex;

static SCHEDULER_TEST_LOCK: Mutex<()> = Mutex::new(());
const REWRITES: u64 = 50;

#[test]
fn an_abandoned_cell_stops_recomputing_while_its_source_is_rewritten_mid_drain() {
    let _serial = SCHEDULER_TEST_LOCK.lock();

    let source = Cell::new(0u64);
    let recomputes = Arc::new(AtomicUsize::new(0));
    let counted = recomputes.clone();
    let derived = source
        .clone()
        .map(move |value| {
            counted.fetch_add(1, Ordering::SeqCst);
            *value
        })
        .materialize();
    let last_holder = Arc::new(Mutex::new(Some(derived)));

    // Rewrites `source` from its own fanout, so each rewrite is queued below
    // `derived` and runs before `derived`'s pending op does. The second round
    // lets go of the only handle to `derived`: by then the first round has
    // queued an op for it whichever subscriber of `source` fired first.
    let rewriter = source.downgrade();
    let holder = last_holder.clone();
    let _guard = source.subscribe(move |signal| {
        if let Signal::Value(value) = signal
            && **value > 0
            && **value < REWRITES
        {
            if **value == 2 {
                holder.lock().take();
            }
            if let Some(source) = rewriter.upgrade() {
                source.set(value.saturating_add(1));
            }
        }
    });

    recomputes.store(0, Ordering::SeqCst);
    batch(|| source.set(1));

    assert!(
        last_holder.lock().is_none(),
        "the handle was never released"
    );
    assert!(
        recomputes.load(Ordering::SeqCst) <= 2,
        "the abandoned cell kept recomputing: {} of {REWRITES} rewrites reached it",
        recomputes.load(Ordering::SeqCst)
    );
}
