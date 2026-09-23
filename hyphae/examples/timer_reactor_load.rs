use std::{env, hint::black_box, thread, time::Duration};

use hyphae::{interval, interval_precise};

fn main() {
    let precise = env::args().nth(1).as_deref() == Some("precise");
    let timer_count = env::args()
        .nth(2)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1_000);
    let period = Duration::from_millis(33);
    let stagger = period
        .checked_div(u32::try_from(timer_count).unwrap_or(u32::MAX))
        .unwrap_or_default();

    let timers: Vec<_> = (0..timer_count)
        .map(|_| {
            let timer = if precise {
                interval_precise(period)
            } else {
                interval(period)
            };
            thread::sleep(stagger);
            timer
        })
        .collect();

    black_box(&timers);
    thread::sleep(Duration::from_secs(10));
}
