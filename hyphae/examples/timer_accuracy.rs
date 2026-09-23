use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use hyphae::{Signal, interval_precise_source};

fn percentile(samples: &[Duration], numerator: usize, denominator: usize) -> Duration {
    let index = samples
        .len()
        .saturating_mul(numerator)
        .checked_div(denominator)
        .unwrap_or_default()
        .min(samples.len().saturating_sub(1));
    samples.get(index).copied().unwrap_or_default()
}

fn main() {
    let period = Duration::from_secs_f64(1.0 / 240.0);
    let timer = interval_precise_source(period);
    let (tx, rx) = mpsc::channel();
    let _guard = timer.subscribe(move |signal| {
        if matches!(signal, Signal::Value(_)) {
            let _ = tx.send(Instant::now());
        }
    });

    let samples: Vec<_> = (0..2_400)
        .filter_map(|_| rx.recv_timeout(Duration::from_secs(1)).ok())
        .collect();
    let mut errors: Vec<_> = samples
        .windows(2)
        .filter_map(|pair| {
            let [first, second] = pair else {
                return None;
            };
            Some(second.duration_since(*first).abs_diff(period))
        })
        .collect();
    errors.sort_unstable();

    println!(
        "samples={} p50_us={} p99_us={} max_us={}",
        errors.len(),
        percentile(&errors, 50, 100).as_micros(),
        percentile(&errors, 99, 100).as_micros(),
        errors.last().copied().unwrap_or_default().as_micros(),
    );
}
