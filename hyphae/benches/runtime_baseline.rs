//! Assertion-backed lifecycle and propagation measurements through the public API.

use std::{
    env,
    hint::black_box,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use hyphae::{
    Cell, CellMap, Gettable, MapExt, MapQuery, Materialize, Mutable, ProjectCellExt, ScanExt,
    Signal, Source, Watchable, batch, join_vec,
};

#[derive(Clone)]
struct Config {
    workload: String,
    size: usize,
    iterations: usize,
    warmup: usize,
}

enum Command {
    Run(Config),
    Help,
}

fn config() -> Result<Command, String> {
    let mut out = Config {
        workload: "all".to_owned(),
        size: 128,
        iterations: 100,
        warmup: 10,
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bench" => {}
            "--workload" => {
                out.workload = args
                    .next()
                    .ok_or_else(|| "--workload needs a value".to_owned())?;
            }
            "--size" => {
                out.size = args
                    .next()
                    .ok_or_else(|| "--size needs a value".to_owned())?
                    .parse()
                    .map_err(|_| "--size must be an integer".to_owned())?;
            }
            "--iterations" => {
                out.iterations = args
                    .next()
                    .ok_or_else(|| "--iterations needs a value".to_owned())?
                    .parse()
                    .map_err(|_| "--iterations must be an integer".to_owned())?;
            }
            "--warmup" => {
                out.warmup = args
                    .next()
                    .ok_or_else(|| "--warmup needs a value".to_owned())?
                    .parse()
                    .map_err(|_| "--warmup must be an integer".to_owned())?;
            }
            "--help" | "-h" => return Ok(Command::Help),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if out.size == 0 {
        return Err("--size must be greater than zero".to_owned());
    }
    if out.iterations == 0 {
        return Err("--iterations must be greater than zero".to_owned());
    }
    Ok(Command::Run(out))
}

#[derive(Default)]
struct Samples {
    setup: Vec<Duration>,
    operation: Vec<Duration>,
    teardown: Vec<Duration>,
}

fn percentile(samples: &mut [Duration], numerator: usize) -> u128 {
    samples.sort_unstable();
    let index = samples
        .len()
        .saturating_sub(1)
        .saturating_mul(numerator)
        .checked_div(100)
        .map_or(0, |value| value);
    samples.get(index).map_or(0, Duration::as_nanos)
}

fn raw_ns(samples: &[Duration]) -> String {
    samples
        .iter()
        .map(|sample| sample.as_nanos().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).map_or(u64::MAX, |converted| converted)
}

fn report(workload: &str, phase: &str, size: usize, samples: &[Duration]) {
    let mut sorted = samples.to_vec();
    let p50 = percentile(&mut sorted, 50);
    let p95 = percentile(&mut sorted, 95);
    let p99 = percentile(&mut sorted, 99);
    println!(
        "{{\"workload\":\"{workload}\",\"phase\":\"{phase}\",\"size\":{size},\"iterations\":{},\"unit\":\"ns\",\"p50\":{p50},\"p95\":{p95},\"p99\":{p99},\"samples\":[{}]}}",
        samples.len(),
        raw_ns(samples)
    );
}

fn report_all(name: &str, size: usize, samples: &Samples) {
    report(name, "setup", size, &samples.setup);
    report(name, "operation", size, &samples.operation);
    report(name, "teardown", size, &samples.teardown);
}

fn lifecycle(cfg: &Config) -> Samples {
    let mut samples = Samples::default();
    for _ in 0..cfg.iterations {
        let start = Instant::now();
        let cells: Vec<_> = (0..cfg.size)
            .map(|index| Cell::new(usize_to_u64(index)))
            .collect();
        let sources: Vec<Source<u64>> = (0..cfg.size).map(|_| Source::new()).collect();
        samples.setup.push(start.elapsed());

        let start = Instant::now();
        let retained_cells = cells.clone();
        let retained_sources = sources.clone();
        black_box((&retained_cells, &retained_sources));
        samples.operation.push(start.elapsed());

        let start = Instant::now();
        drop(retained_cells);
        drop(retained_sources);
        drop(cells);
        drop(sources);
        samples.teardown.push(start.elapsed());
    }
    samples
}

fn deep_chain(cfg: &Config) -> Samples {
    let mut samples = Samples::default();
    for iteration in 0..cfg.iterations {
        let start = Instant::now();
        let source = Cell::new(0_u64);
        let mut tail = source
            .clone()
            .map(|value| value.wrapping_add(1))
            .materialize();
        for _ in 1..cfg.size {
            tail = tail.map(|value| value.wrapping_add(1)).materialize();
        }
        samples.setup.push(start.elapsed());

        let input = usize_to_u64(iteration).wrapping_add(1);
        let start = Instant::now();
        source.set(input);
        let output = tail.get();
        samples.operation.push(start.elapsed());
        assert_eq!(output, input.wrapping_add(usize_to_u64(cfg.size)));

        let start = Instant::now();
        drop(tail);
        drop(source);
        samples.teardown.push(start.elapsed());
    }
    samples
}

fn wide_diamond(cfg: &Config) -> Samples {
    let mut samples = Samples::default();
    for iteration in 0..cfg.iterations {
        let start = Instant::now();
        let source = Cell::new(0_u64);
        let legs: Vec<_> = (0..cfg.size)
            .map(|offset| {
                source
                    .clone()
                    .map(move |value| value.wrapping_add(usize_to_u64(offset)))
                    .materialize()
            })
            .collect();
        let joined = join_vec(legs).materialize();
        let solves = Arc::new(AtomicU64::new(0));
        let solve_counter = solves.clone();
        let sink = joined
            .clone()
            .map(move |values| {
                solve_counter.fetch_add(1, Ordering::Relaxed);
                values.iter().copied().fold(0_u64, u64::wrapping_add)
            })
            .materialize();
        let guard = sink.subscribe(|_| {});
        solves.store(0, Ordering::Relaxed);
        samples.setup.push(start.elapsed());

        let input = usize_to_u64(iteration).wrapping_add(1);
        let start = Instant::now();
        batch(|| source.set(input));
        let output = sink.get();
        samples.operation.push(start.elapsed());
        let n = usize_to_u64(cfg.size);
        let offsets = n
            .wrapping_mul(n.saturating_sub(1))
            .checked_div(2)
            .map_or(0, |value| value);
        assert_eq!(output, n.wrapping_mul(input).wrapping_add(offsets));
        assert_eq!(
            solves.load(Ordering::Relaxed),
            1,
            "batched diamond solved more than once"
        );

        let start = Instant::now();
        drop(guard);
        drop(sink);
        drop(joined);
        drop(source);
        samples.teardown.push(start.elapsed());
    }
    samples
}

fn event_no_coalesce(cfg: &Config) -> Samples {
    let mut samples = Samples::default();
    for iteration in 0..cfg.iterations {
        let start = Instant::now();
        let (source, count) = hyphae::scheduler::no_coalesce(|| {
            let source = Cell::new(0_u64);
            let count = source
                .clone()
                .scan(0_u64, |n, _| n.wrapping_add(1))
                .materialize();
            (source, count)
        });
        let hits = Arc::new(AtomicU64::new(0));
        let hit_counter = hits.clone();
        let guard = count.subscribe(move |signal| {
            if matches!(signal, Signal::Value(_)) {
                hit_counter.fetch_add(1, Ordering::Relaxed);
            }
        });
        hits.store(0, Ordering::Relaxed);
        samples.setup.push(start.elapsed());

        let size = usize_to_u64(cfg.size);
        let base = usize_to_u64(iteration).wrapping_mul(size);
        let start = Instant::now();
        batch(|| {
            for offset in 0..cfg.size {
                source.set(base.wrapping_add(usize_to_u64(offset)).wrapping_add(1));
            }
        });
        let output = count.get();
        samples.operation.push(start.elapsed());
        assert_eq!(output, size.wrapping_add(1), "scan lost an input event");
        assert_eq!(
            hits.load(Ordering::Relaxed),
            size,
            "observer lost a scan output"
        );

        let start = Instant::now();
        drop(guard);
        drop(count);
        drop(source);
        samples.teardown.push(start.elapsed());
    }
    samples
}

fn source_fanout(cfg: &Config) -> Samples {
    let mut samples = Samples::default();
    for iteration in 0..cfg.iterations {
        let start = Instant::now();
        let source = Source::<u64>::new();
        let hits = Arc::new(AtomicU64::new(0));
        let mismatches = Arc::new(AtomicU64::new(0));
        let input = usize_to_u64(iteration);
        let guards: Vec<_> = (0..cfg.size)
            .map(|_| {
                let hit_counter = hits.clone();
                let mismatch_counter = mismatches.clone();
                source.subscribe(move |signal| {
                    if let Signal::Value(value) = signal {
                        hit_counter.fetch_add(1, Ordering::Relaxed);
                        if **value != input {
                            mismatch_counter.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        samples.setup.push(start.elapsed());

        let start = Instant::now();
        source.emit(input);
        samples.operation.push(start.elapsed());
        assert_eq!(hits.load(Ordering::Relaxed), usize_to_u64(cfg.size));
        assert_eq!(mismatches.load(Ordering::Relaxed), 0);

        let start = Instant::now();
        drop(guards);
        drop(source);
        samples.teardown.push(start.elapsed());
    }
    samples
}

fn project_cell_churn(cfg: &Config) -> Samples {
    let mut samples = Samples::default();
    for iteration in 0..cfg.iterations {
        let start = Instant::now();
        let rows = CellMap::<u64, u64>::new();
        let weights = CellMap::<u64, u64>::new();
        let size = usize_to_u64(cfg.size);
        for key in 0..size {
            rows.insert(key, key);
            weights.insert(key, 1);
        }
        let weights_for_mapper = weights.clone();
        let projected = rows
            .clone()
            .project_cell(move |key, value| {
                let key = *key;
                let value = *value;
                weights_for_mapper
                    .get(&key)
                    .map(move |weight| {
                        let weight = weight.as_ref().map_or(0, |weight| *weight);
                        Some((key, value.wrapping_mul(weight)))
                    })
                    .materialize()
            })
            .materialize();
        samples.setup.push(start.elapsed());

        let generation = usize_to_u64(iteration).wrapping_add(2);
        let start = Instant::now();
        for key in 0..size {
            rows.insert(key, key.wrapping_add(generation));
        }
        let last = size.saturating_sub(1);
        let output = projected.get_value(&last);
        samples.operation.push(start.elapsed());
        assert_eq!(output, Some(last.wrapping_add(generation)));
        for key in 0..size {
            assert_eq!(
                projected.get_value(&key),
                Some(key.wrapping_add(generation))
            );
        }

        let start = Instant::now();
        drop(projected);
        drop(weights);
        drop(rows);
        samples.teardown.push(start.elapsed());
    }
    samples
}

fn selected(cfg: &Config, name: &str) -> bool {
    cfg.workload == "all" || cfg.workload == name
}

fn main() -> Result<(), String> {
    let cfg = match config()? {
        Command::Run(cfg) => cfg,
        Command::Help => {
            println!(
                "usage: runtime_baseline [--workload all|lifecycle|deep_chain|wide_diamond|event_no_coalesce|source_fanout|project_cell_churn] [--size N] [--iterations N] [--warmup N]"
            );
            return Ok(());
        }
    };
    let workloads: [(&str, fn(&Config) -> Samples); 6] = [
        ("lifecycle", lifecycle),
        ("deep_chain", deep_chain),
        ("wide_diamond", wide_diamond),
        ("event_no_coalesce", event_no_coalesce),
        ("source_fanout", source_fanout),
        ("project_cell_churn", project_cell_churn),
    ];
    let mut matched = false;
    for (name, run) in workloads {
        if selected(&cfg, name) {
            matched = true;
            if cfg.warmup > 0 {
                let mut warmup = cfg.clone();
                warmup.iterations = cfg.warmup;
                black_box(run(&warmup));
            }
            report_all(name, cfg.size, &run(&cfg));
        }
    }
    if !matched {
        return Err(format!("unknown workload: {}", cfg.workload));
    }
    Ok(())
}
