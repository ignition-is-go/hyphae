//! Long-running hyphae demo with many cells changing value over time.
//!
//! Starts an inspector server and creates a graph of interconnected cells
//! driven by interval timers at various rates. Connect the hyphae-inspector
//! TUI to the printed port to visualize the live cell graph.

use std::sync::Arc;
use std::time::Duration;

use hyphae::{
    Cell, CellImmutable, CellMap, FilterExt, JoinExt, MapExt, PairwiseExt, Pipeline, ScanExt,
    Signal, SwitchMapExt, Watchable, WindowExt, interval, join_vec,
};

#[tokio::main]
async fn main() {
    let server = hyphae::server::start_server("demo");
    println!("cargo run -p hyphae-inspector -- :{}", server.port());

    // ── Source clocks at different rates ─────────────────────────────────

    let fast = interval(Duration::from_millis(100)).with_name("fast_100ms");
    let medium = interval(Duration::from_millis(500)).with_name("medium_500ms");
    let slow = interval(Duration::from_millis(2000)).with_name("slow_2s");

    // ── Derived: sine + cosine from fast clock ──────────────────────────

    let sine = fast
        .clone()
        .map(|tick| {
            let t = *tick as f64 * 0.1;
            (t.sin() * 100.0).round() / 100.0
        })
        .materialize()
        .with_name("sine");

    let cosine = fast
        .clone()
        .map(|tick| {
            let t = *tick as f64 * 0.1;
            (t.cos() * 100.0).round() / 100.0
        })
        .materialize()
        .with_name("cosine");

    // ── Magnitude from sine+cosine (should always be ~1.0) ──────────────

    let _magnitude = sine
        .join(&cosine)
        .map(|(s, c)| ((s * s + c * c).sqrt() * 1000.0).round() / 1000.0)
        .materialize()
        .with_name("magnitude");

    // ── Running average of sine using scan ───────────────────────────────

    let _running_avg = sine
        .scan((0.0_f64, 0_u64), |state, value| {
            let (sum, count) = state;
            (sum + value, count + 1)
        })
        .map(|(sum, count)| {
            if *count == 0 {
                0.0
            } else {
                (sum / *count as f64 * 1000.0).round() / 1000.0
            }
        })
        .materialize()
        .with_name("running_avg");

    // ── Fibonacci sequence on medium clock ───────────────────────────────

    let fib = medium
        .scan((0_u64, 1_u64), |state, _| {
            let (a, b) = state;
            (*b, a.wrapping_add(*b))
        })
        .map(|(a, _)| *a)
        .materialize()
        .with_name("fibonacci");

    // ── Phase selector on slow clock ────────────────────────────────────

    let phase = slow
        .map(|tick| match tick % 4 {
            0 => "rising",
            1 => "peak",
            2 => "falling",
            _ => "trough",
        })
        .materialize()
        .with_name("phase");

    // ── Switch map: signal shape depends on phase ───────────────────────

    let fast_for_switch = fast.clone();
    let _phase_signal = phase
        .switch_map(move |phase_name| match *phase_name {
            "rising" => fast_for_switch.clone().map(|t| (*t as f64 * 0.05).min(1.0)).materialize(),
            "peak" => Cell::new(1.0).lock(),
            "falling" => fast_for_switch.clone().map(|t| (1.0 - *t as f64 * 0.05).max(0.0)).materialize(),
            _ => Cell::new(0.0).lock(),
        })
        .with_name("phase_signal");

    // ── Filter: only positive sine values ───────────────────────────────

    let _positive_sine = sine
        .clone()
        .filter(|v| *v > 0.0)
        .materialize()
        .with_name("positive_sine");

    // ── Pairwise velocity ───────────────────────────────────────────────

    let velocity = sine
        .pairwise()
        .map(|(prev, curr)| ((curr - prev) * 1000.0).round() / 1000.0)
        .materialize()
        .with_name("velocity");

    // ── Sliding window of last 5 fibonacci values ───────────────────────

    let _fib_window = fib.window(5).with_name("fib_window");

    // ── CellMap scoreboard driven by multiple sources ───────────────────

    let scoreboard: CellMap<Arc<str>, f64> = CellMap::new();

    let sb = scoreboard.clone();
    let _g1 = sine.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            sb.insert("sine".into(), **v);
        }
    });

    let sb = scoreboard.clone();
    let _g2 = cosine.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            sb.insert("cosine".into(), **v);
        }
    });

    let _score_entries = scoreboard.entries().with_name("scoreboard");
    let _score_len = scoreboard.len().with_name("scoreboard_len");

    // ── join_vec: combine 5 signals into a stats pipeline ───────────────

    let all_signals: Vec<Cell<f64, CellImmutable>> = vec![
        sine.clone(),
        cosine.clone(),
        _magnitude.clone(),
        velocity.clone(),
    ];
    let combined = join_vec(all_signals).with_name("all_signals");

    let _stats = combined
        .map(|values| {
            let min = values.iter().cloned().reduce(f64::min).unwrap_or(0.0);
            let max = values.iter().cloned().reduce(f64::max).unwrap_or(0.0);
            let sum: f64 = values.iter().sum();
            let avg = if values.is_empty() {
                0.0
            } else {
                sum / values.len() as f64
            };
            (
                (min * 100.0).round() / 100.0,
                (max * 100.0).round() / 100.0,
                (avg * 100.0).round() / 100.0,
            )
        })
        .materialize()
        .with_name("stats");

    // ── Run forever ─────────────────────────────────────────────────────

    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
