use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use hyphae::{Cell, Gettable, Mutable, Signal, Watchable};

#[test]
#[ignore = "known concurrent notification ordering defect: lv-9621"]
fn a_slow_observer_cannot_strand_another_observer_at_an_older_value() -> Result<(), String> {
    let source = Cell::new(0_usize);
    let first = Arc::new(AtomicBool::new(false));
    let latest = [Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))];
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let mut guards = Vec::new();
    for seen in &latest {
        let seen = seen.clone();
        let first = first.clone();
        let entered = entered_tx.clone();
        let release = release_rx.clone();
        guards.push(source.subscribe(move |signal| {
            let Signal::Value(value) = signal else {
                return;
            };
            if **value == 1 && !first.swap(true, Ordering::AcqRel) {
                let _ = entered.send(());
                let _ = release
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .recv_timeout(Duration::from_secs(5));
            }
            seen.store(**value, Ordering::Release);
        }));
    }
    let sender = source.clone();
    let one = std::thread::spawn(move || sender.set(1));
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|e| format!("first callback never entered: {e}"))?;
    let (done_tx, done_rx) = mpsc::channel();
    let sender = source.clone();
    let two = std::thread::spawn(move || {
        sender.set(2);
        let _ = done_tx.send(());
    });
    // Accept an implementation that blocks the concurrent writer as well as
    // one that queues it. Neither may deliver stale state after both return.
    let _ = done_rx.recv_timeout(Duration::from_secs(1));
    let _ = release_tx.send(());
    one.join().map_err(|_| "first writer panicked")?;
    two.join().map_err(|_| "second writer panicked")?;
    let current = source.get();
    for (index, seen) in latest.iter().enumerate() {
        let value = seen.load(Ordering::Acquire);
        if value != current {
            return Err(format!(
                "observer {index} stranded at {value}, source is {current}"
            ));
        }
    }
    drop(guards);
    Ok(())
}
