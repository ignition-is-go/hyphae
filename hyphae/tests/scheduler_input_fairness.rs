//! Input that arrives during a drain must not hold back work that was already
//! admitted. An integration test because the scheduler is process-wide.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};

use hyphae::{Cell, MapExt, Materialize, Mutable, Signal, Watchable, batch};

const ARRIVALS: usize = 50;
const DEPTH: usize = 40;

#[test]
fn deep_work_runs_before_input_that_arrived_after_it() {
    let shallow = Cell::new(0_i64);
    let deep_source = Cell::new(0_i64);
    let mut deep = deep_source.clone().map(|value| *value).materialize();
    for _ in 0..DEPTH {
        deep = deep.map(|value| value + 1).materialize();
    }

    // Another thread writes `shallow` on request, the way a socket thread
    // delivers the next input while a drain is in flight.
    let (request, requests) = mpsc::channel::<i64>();
    let (done, acks) = mpsc::channel::<()>();
    let writer_cell = shallow.clone();
    let writer = std::thread::spawn(move || {
        for value in requests {
            batch(|| writer_cell.set(value));
            let _ = done.send(());
        }
    });

    let armed = Arc::new(AtomicBool::new(false));
    let shallow_runs = Arc::new(AtomicUsize::new(0));
    let (runs, shallow_armed) = (shallow_runs.clone(), armed.clone());
    let acks = std::sync::Mutex::new(acks);
    let request = std::sync::Mutex::new(Some(request));
    let _shallow = shallow.subscribe(move |signal| {
        if !shallow_armed.load(Ordering::SeqCst) || !matches!(signal, Signal::Value(_)) {
            return;
        }
        let run = runs.fetch_add(1, Ordering::SeqCst) + 1;
        let mut request = request.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if run >= ARRIVALS {
            request.take();
            return;
        }
        if let Some(sender) = request.as_ref() {
            let next = i64::try_from(run).unwrap_or(i64::MAX) + 1;
            sender.send(next).expect("writer thread is alive");
            acks.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv()
                .expect("writer thread acknowledges");
        }
    });

    let shallow_runs_when_deep_ran = Arc::new(AtomicUsize::new(usize::MAX));
    let (seen, runs, deep_armed) = (
        shallow_runs_when_deep_ran.clone(),
        shallow_runs.clone(),
        armed.clone(),
    );
    let _deep = deep.subscribe(move |signal| {
        if deep_armed.load(Ordering::SeqCst) && matches!(signal, Signal::Value(_)) {
            let _ = seen.compare_exchange(
                usize::MAX,
                runs.load(Ordering::SeqCst),
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
    });

    armed.store(true, Ordering::SeqCst);
    batch(|| {
        shallow.set(1);
        deep_source.set(1);
    });
    writer.join().expect("writer thread exits");

    assert_eq!(
        shallow_runs.load(Ordering::SeqCst),
        ARRIVALS,
        "every arrival was delivered"
    );
    let waited = shallow_runs_when_deep_ran.load(Ordering::SeqCst);
    assert!(
        waited <= 1,
        "the deep chain waited for {waited} later arrivals; it was admitted with the first"
    );
}
