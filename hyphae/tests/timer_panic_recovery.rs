#![cfg(not(target_arch = "wasm32"))]

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use hyphae::{Signal, interval_precise_with_elapsed_source};

#[test]
#[allow(clippy::panic)]
fn interval_resumes_after_subscriber_panics() {
    let source = interval_precise_with_elapsed_source(Duration::from_millis(10));
    #[cfg(feature = "scheduler")]
    let source = source.batched();
    let panicked = AtomicBool::new(false);
    let (panic_tx, panic_rx) = mpsc::channel();
    let _panicking = source.subscribe(move |signal| {
        if matches!(signal, Signal::Value(_)) && !panicked.swap(true, Ordering::SeqCst) {
            let _sent = panic_tx.send(());
            panic!("transient interval subscriber failure");
        }
    });
    let (tick_tx, tick_rx) = mpsc::channel();
    let _healthy = source.subscribe(move |signal| {
        if let Signal::Value(tick) = signal {
            let _sent = tick_tx.send(*tick.as_ref());
        }
    });

    assert_eq!(panic_rx.recv_timeout(Duration::from_secs(2)), Ok(()));
    let first = tick_rx.recv_timeout(Duration::from_secs(2));
    assert!(
        first.is_ok(),
        "shared interval stopped after a subscriber panic: {first:?}"
    );
    let second = tick_rx.recv_timeout(Duration::from_secs(2));
    assert!(
        matches!((first, second), (Ok(first), Ok(second)) if second.tick > first.tick && second.elapsed > first.elapsed),
        "shared interval must continue advancing after recovery"
    );
}
