use std::{
    env,
    hint::black_box,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use hyphae::{
    CellMap, MapDiff, MapQuery, Materialize, Signal, Watchable,
    traits::{Gettable, MapValuesExt},
};

const KEY: u64 = 0;

#[derive(Clone, Copy)]
enum Case {
    NoKeyObservers,
    LiveKeyObserver,
    Query,
}

impl Case {
    const fn name(self) -> &'static str {
        match self {
            Self::NoKeyObservers => "no-key-observers",
            Self::LiveKeyObserver => "live-key-observer",
            Self::Query => "query",
        }
    }
}

struct Measurement {
    case: Case,
    iterations: u64,
    elapsed_ns: u128,
    delivered_diffs: u64,
    key_events: u64,
    source_diff_subscribers: u64,
    output_diff_subscribers: u64,
    key_observers: u64,
}

struct DiffCheck {
    next: Arc<AtomicU64>,
    mismatches: Arc<AtomicU64>,
    saw_initial: Arc<AtomicBool>,
}

fn checked_diff_counter(value_offset: u64) -> (DiffCheck, impl Fn(&MapDiff<u64, u64>)) {
    let check = DiffCheck {
        next: Arc::new(AtomicU64::new(1)),
        mismatches: Arc::new(AtomicU64::new(0)),
        saw_initial: Arc::new(AtomicBool::new(false)),
    };
    let callback_next = Arc::clone(&check.next);
    let callback_mismatches = Arc::clone(&check.mismatches);
    let callback_saw_initial = Arc::clone(&check.saw_initial);
    let callback = move |diff: &MapDiff<u64, u64>| match diff {
        MapDiff::Initial { entries } => {
            if callback_saw_initial.swap(true, Ordering::Relaxed)
                || entries.as_slice() != [(KEY, value_offset)]
            {
                callback_mismatches.fetch_add(1, Ordering::Relaxed);
            }
        }
        MapDiff::Update {
            key,
            old_value,
            new_value,
        } => {
            let expected = callback_next.fetch_add(1, Ordering::Relaxed);
            let expected_old = expected.saturating_sub(1).saturating_add(value_offset);
            let expected_new = expected.saturating_add(value_offset);
            if *key != KEY || *old_value != expected_old || *new_value != expected_new {
                callback_mismatches.fetch_add(1, Ordering::Relaxed);
            }
        }
        MapDiff::Insert { .. } | MapDiff::Remove { .. } | MapDiff::Batch { .. } => {
            callback_mismatches.fetch_add(1, Ordering::Relaxed);
        }
    };
    (check, callback)
}

fn verify_diffs(check: &DiffCheck, iterations: u64) -> u64 {
    let delivered = check.next.load(Ordering::Relaxed).saturating_sub(1);
    assert!(check.saw_initial.load(Ordering::Relaxed));
    assert_eq!(check.mismatches.load(Ordering::Relaxed), 0);
    assert_eq!(delivered, iterations);
    delivered
}

fn apply_updates(source: &CellMap<u64, u64>, iterations: u64) -> u128 {
    let started = Instant::now();
    for new_value in 1..=iterations {
        source.apply_diff_owned(MapDiff::Update {
            key: KEY,
            old_value: new_value.saturating_sub(1),
            new_value: black_box(new_value),
        });
    }
    started.elapsed().as_nanos()
}

fn run_no_key_observers(iterations: u64) -> Measurement {
    let source = CellMap::<u64, u64>::new();
    source.insert(KEY, 0);
    let (diff_check, check_diff) = checked_diff_counter(0);
    let _diff_guard = source.subscribe_diffs(check_diff);

    let elapsed_ns = apply_updates(&source, iterations);
    let delivered_diffs = verify_diffs(&diff_check, iterations);
    assert_eq!(source.get_value(&KEY), Some(iterations));

    Measurement {
        case: Case::NoKeyObservers,
        iterations,
        elapsed_ns,
        delivered_diffs,
        key_events: 0,
        source_diff_subscribers: 1,
        output_diff_subscribers: 0,
        key_observers: 0,
    }
}

fn run_live_key_observer(iterations: u64) -> Measurement {
    let source = CellMap::<u64, u64>::new();
    source.insert(KEY, 0);
    let (diff_check, check_diff) = checked_diff_counter(0);
    let _diff_guard = source.subscribe_diffs(check_diff);
    let watched = source.get(&KEY).materialize();
    let next_key_value = Arc::new(AtomicU64::new(0));
    let key_mismatches = Arc::new(AtomicU64::new(0));
    let callback_next_key_value = Arc::clone(&next_key_value);
    let callback_key_mismatches = Arc::clone(&key_mismatches);
    let _key_guard = watched.subscribe(move |signal| {
        let expected = callback_next_key_value.fetch_add(1, Ordering::Relaxed);
        if !matches!(signal, Signal::Value(value) if value.as_ref() == &Some(expected)) {
            callback_key_mismatches.fetch_add(1, Ordering::Relaxed);
        }
    });

    let elapsed_ns = apply_updates(&source, iterations);
    let delivered_diffs = verify_diffs(&diff_check, iterations);
    let key_values = next_key_value.load(Ordering::Relaxed);
    assert_eq!(key_mismatches.load(Ordering::Relaxed), 0);
    assert_eq!(key_values, iterations.saturating_add(1));
    assert_eq!(source.get_value(&KEY), Some(iterations));
    assert_eq!(watched.get(), Some(iterations));

    Measurement {
        case: Case::LiveKeyObserver,
        iterations,
        elapsed_ns,
        delivered_diffs,
        key_events: key_values.saturating_sub(1),
        source_diff_subscribers: 1,
        output_diff_subscribers: 0,
        key_observers: 1,
    }
}

fn run_query(iterations: u64) -> Measurement {
    let source = CellMap::<u64, u64>::new();
    source.insert(KEY, 0);
    let output = source
        .clone()
        .map_values(|_, value| value.saturating_add(1))
        .materialize();
    let (diff_check, check_diff) = checked_diff_counter(1);
    let _output_guard = output.subscribe_diffs(check_diff);

    let elapsed_ns = apply_updates(&source, iterations);
    let delivered_diffs = verify_diffs(&diff_check, iterations);
    assert_eq!(source.get_value(&KEY), Some(iterations));
    assert_eq!(output.get_value(&KEY), Some(iterations.saturating_add(1)));

    Measurement {
        case: Case::Query,
        iterations,
        elapsed_ns,
        delivered_diffs,
        key_events: 0,
        source_diff_subscribers: 1,
        output_diff_subscribers: 1,
        key_observers: 0,
    }
}

fn print_measurement(measurement: &Measurement) {
    println!(
        "case={} iterations={} elapsed_ns={} delivered_diffs={} key_events={} source_diff_subscribers={} output_diff_subscribers={} key_observers={}",
        measurement.case.name(),
        measurement.iterations,
        measurement.elapsed_ns,
        measurement.delivered_diffs,
        measurement.key_events,
        measurement.source_diff_subscribers,
        measurement.output_diff_subscribers,
        measurement.key_observers,
    );
}

fn parse_iterations(value: Option<String>) -> Result<u64, String> {
    let raw = value.unwrap_or_else(|| "1000000".to_owned());
    let iterations = raw
        .parse::<u64>()
        .map_err(|error| format!("invalid iteration count {raw:?}: {error}"))?;
    if iterations == 0 {
        return Err("iteration count must be greater than zero".to_owned());
    }
    Ok(iterations)
}

fn main() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let selected = args.next().unwrap_or_else(|| "all".to_owned());
    let iterations = parse_iterations(args.next())?;
    if let Some(extra) = args.next() {
        return Err(format!("unexpected argument {extra:?}"));
    }

    match selected.as_str() {
        "no-key-observers" => print_measurement(&run_no_key_observers(iterations)),
        "live-key-observer" => print_measurement(&run_live_key_observer(iterations)),
        "query" => print_measurement(&run_query(iterations)),
        "all" => {
            print_measurement(&run_no_key_observers(iterations));
            print_measurement(&run_live_key_observer(iterations));
            print_measurement(&run_query(iterations));
        }
        _ => {
            return Err(format!(
                "unknown case {selected:?}; expected no-key-observers, live-key-observer, query, or all"
            ));
        }
    }
    Ok(())
}
