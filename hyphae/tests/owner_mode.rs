//! PROTOTYPE checks for `HYPHAE_OWNER=1`. Skipped unless the variable is set.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use hyphae::{Cell, Gettable, JoinExt, MapExt, Materialize, Mutable, Signal, Watchable, batch};

fn owner() -> bool {
    std::env::var("HYPHAE_OWNER").is_ok_and(|value| value == "1")
}

#[test]
fn a_write_posts_and_settles_on_the_owner_thread() {
    if !owner() {
        return;
    }
    let source = Cell::new(1_i64);
    let left = source.clone().map(|value| value + 1).materialize();
    let right = source.clone().map(|value| value * 10).materialize();
    let sum = left.join(right).map(|(l, r)| l + r).materialize();
    let fires = Arc::new(AtomicUsize::new(0));
    let seen = fires.clone();
    let thread = Arc::new(std::sync::Mutex::new(None));
    let ran_on = thread.clone();
    let _guard = sum.subscribe(move |signal| {
        if matches!(signal, Signal::Value(_)) {
            seen.fetch_add(1, Ordering::SeqCst);
            *ran_on.lock().unwrap() = std::thread::current().name().map(str::to_owned);
        }
    });
    hyphae::scheduler::settle();
    let before = fires.load(Ordering::SeqCst);
    batch(|| {
        source.set(2);
        source.set(3);
    });
    hyphae::scheduler::settle();
    assert_eq!(sum.get(), 34);
    assert_eq!(fires.load(Ordering::SeqCst), before + 1, "one glitch-free emission");
    assert_eq!(thread.lock().unwrap().as_deref(), Some("hyphae-owner"));
}
