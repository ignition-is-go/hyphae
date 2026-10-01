#![cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]

use std::sync::Arc;

use hyphae::{Cell, Materialize, Mutable, Signal, TakeExt, Watchable, batch};
use parking_lot::Mutex;

#[derive(Debug, PartialEq, Eq)]
enum Observed {
    Value(u64),
    Complete,
    Error,
}

#[test]
fn parallel_take_delivers_the_final_value_before_completion() {
    hyphae::scheduler::set_wave_threshold_for_test(4);
    let mut sources = Vec::new();
    let mut observations = Vec::new();
    let mut outputs = Vec::new();
    let mut guards = Vec::new();
    for _ in 0..128 {
        let source = Cell::new(0_u64);
        let output = source.clone().take(2).materialize();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&observed);
        guards.push(output.subscribe(move |signal| {
            sink.lock().push(match signal {
                Signal::Value(value) => Observed::Value(**value),
                Signal::Complete => Observed::Complete,
                Signal::Error(_) => Observed::Error,
            });
        }));
        observed.lock().clear();
        sources.push(source);
        observations.push(observed);
        outputs.push(output);
    }

    batch(|| {
        for source in &sources {
            source.set(1);
        }
    });

    for observed in &observations {
        assert_eq!(
            *observed.lock(),
            vec![Observed::Value(1), Observed::Complete]
        );
    }
    batch(|| {
        for source in &sources {
            source.set(2);
        }
    });
    for observed in &observations {
        assert_eq!(
            *observed.lock(),
            vec![Observed::Value(1), Observed::Complete]
        );
    }
    drop((guards, outputs));
}
