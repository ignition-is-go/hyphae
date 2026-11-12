#![cfg(feature = "profiling")]
use std::sync::{Arc, Mutex};

use hyphae::{Cell, Gettable, Mutable, Signal, Watchable};

#[test]
fn equal_values_cut_off_but_unique_occurrences_and_terminal_signals_survive() {
    let enabled = std::env::var("HYPHAE_EQ_CUTOFF").is_ok_and(|v| v == "1");
    let cell = Cell::new(0);
    let values = Arc::new(Mutex::new(Vec::new()));
    let completed = Arc::new(Mutex::new(false));
    let seen = values.clone();
    let done = completed.clone();
    let guard = cell.subscribe(move |signal| match signal {
        Signal::Value(value) => seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(**value),
        Signal::Complete => {
            *done
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        }
        Signal::Error(_) => {}
    });
    cell.set(0);
    cell.set(1);
    cell.set(1);
    cell.notify(Signal::Complete);
    assert_eq!(
        *values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        if enabled {
            vec![0, 1]
        } else {
            vec![0, 0, 1, 1]
        }
    );
    assert_eq!(cell.get(), 1);
    assert!(
        *completed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    );
    drop(guard);

    // An occurrence ID retains individuality even when the payload is equal.
    let events = Cell::new((0_u64, "same"));
    let received = Arc::new(Mutex::new(Vec::new()));
    let seen = received.clone();
    let guard = events.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(**value);
        }
    });
    events.set((1, "same"));
    events.set((2, "same"));
    assert_eq!(
        *received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![(0, "same"), (1, "same"), (2, "same")]
    );
    drop(guard);
}

#[cfg(feature = "scheduler")]
#[test]
fn scheduled_values_use_the_same_cutoff_and_preserve_final_value_before_completion() {
    let cell = Cell::new(0);
    let values = Arc::new(Mutex::new(Vec::new()));
    let seen = values.clone();
    let guard = cell.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(**value);
        }
    });
    hyphae::scheduler::batch(|| {
        cell.set(0);
    });
    hyphae::scheduler::batch(|| {
        cell.set(1);
        cell.notify(Signal::Complete);
    });
    let enabled = std::env::var("HYPHAE_EQ_CUTOFF").is_ok_and(|v| v == "1");
    assert_eq!(
        *values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        if enabled { vec![0, 1] } else { vec![0, 0, 1] }
    );
    drop(guard);
}

#[test]
fn occurrence_ids_keep_sampling_ticks_functional() {
    use hyphae::{Materialize, SampleExt};
    let source = Cell::new(0);
    let ticks = Cell::new(0_u64);
    let sampled = source.sample(&ticks).materialize();
    source.set(3);
    ticks.set(1);
    assert_eq!(sampled.get(), 3);
    source.set(5);
    ticks.set(2);
    assert_eq!(sampled.get(), 5);
}
