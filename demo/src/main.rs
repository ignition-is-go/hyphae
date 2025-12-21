use rx3::{from_iter_with_delay, interval, Watchable};
use std::time::Duration;

fn main() {
    let items = from_iter_with_delay(vec!["a", "b", "c"], Duration::from_millis(300));
    items.watch(|v| println!("iter: {}", v));

    let ticker = interval(Duration::from_millis(500));
    ticker.watch(|v| println!("tick: {}", v));

    std::thread::sleep(Duration::from_secs(2));
}
