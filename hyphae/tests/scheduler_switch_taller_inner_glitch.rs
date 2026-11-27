//! A diamond whose one branch switches onto a taller inner after the join was
//! built must stay glitch-free: the join has to be raised above the new inner
//! so a batched write reaches both join inputs before the join runs.
//!
//! This lives in its own test binary because it opens root `batch()`es. The
//! root tick is process-wide: while a batch or its drain is in flight, every
//! other thread's `set` is deferred to that drain (and coalesced with later
//! writes to the same cell). In the library's unit-test binary that made
//! unrelated tests running beside it observe their own writes late or not at
//! all.

use std::sync::Arc;

use hyphae::{
    Cell, Gettable, JoinExt, MapExt, Materialize, Mutable, Signal, SwitchMapExt, Watchable, batch,
};
use parking_lot::Mutex;

#[test]
fn switch_to_taller_inner_keeps_diamond_glitch_free() {
    let source = Cell::new(0u64);
    let mut deep = source.clone().map(|x| *x).materialize();
    for _ in 0..4 {
        deep = deep.clone().map(|x| *x).materialize();
    }
    let selector = Cell::new(false);
    let (shallow, tall) = (source.clone().lock(), deep);
    let right = selector
        .clone()
        .switch_map(move |use_tall: &bool| {
            if *use_tall {
                tall.clone()
            } else {
                shallow.clone()
            }
        })
        .materialize();
    let left = source.clone().map(|x| *x).materialize();
    let joined = left.join(right).materialize();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let _guard = joined.subscribe(move |signal| {
        if let Signal::Value(pair) = signal {
            sink.lock().push(**pair);
        }
    });
    selector.set(true);
    seen.lock().clear();
    for i in 1..=20 {
        batch(|| source.set(i));
    }
    let seen = std::mem::take(&mut *seen.lock());
    assert!(
        seen.iter().all(|(l, r)| l == r),
        "glitch through diamond: {seen:?}"
    );
    assert_eq!(joined.get(), (20, 20));
}
