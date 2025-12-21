use rx3::{MapExt, MergeMapExt, SwitchMapExt, Watchable, from_iter_with_delay, interval};
use std::time::{Duration, Instant};

fn main() {
    let start = Instant::now();

    let ticker = interval(Duration::from_millis(1000)).with_name("numbers");

    let mapped = ticker.switch_map(|v| {
        let v = *v;
        from_iter_with_delay(["a", "b", "c"], Duration::from_millis(600))
            .map(move |&letter| format!("{}{}", v, letter))
    });

    mapped.watch(move |x| {
        let elapsed = start.elapsed();
        println!(
            "{}.{:03}s: {}",
            elapsed.as_secs(),
            elapsed.subsec_millis(),
            x
        );
    });

    std::thread::sleep(Duration::from_secs(10));
}
