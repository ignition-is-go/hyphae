//! Clock, pacing, histograms, process and machine counters, and the metric
//! map every scenario writes into.

use std::{
    collections::BTreeMap,
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

static BASE: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Nanoseconds since the process clock started. Never 0 after `init_clock`.
pub fn now() -> u64 {
    u64::try_from(BASE.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

pub fn init_clock() {
    let _ = *BASE;
    std::thread::sleep(Duration::from_micros(10));
}

/// Sleep until `target` on the process clock: coarse sleep, then spin.
pub fn sleep_until(target: u64) {
    loop {
        let t = now();
        if t >= target {
            return;
        }
        let left = target - t;
        if left > 200_000 {
            std::thread::sleep(Duration::from_nanos(left - 150_000));
        } else {
            std::hint::spin_loop();
        }
    }
}

/// Spin until `done` holds or `timeout_ns` passes. True when it held.
pub fn wait_for(timeout_ns: u64, done: impl Fn() -> bool) -> bool {
    let deadline = now().saturating_add(timeout_ns);
    let mut spins = 0u32;
    loop {
        if done() {
            return true;
        }
        if now() > deadline {
            return false;
        }
        spins = spins.wrapping_add(1);
        if spins.is_multiple_of(64) {
            std::thread::yield_now();
        } else {
            std::hint::spin_loop();
        }
    }
}

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[derive(Default, Clone)]
pub struct Hist(pub Vec<u64>);

impl Hist {
    pub fn add(&mut self, ns: u64) {
        self.0.push(ns);
    }

    pub fn q(&self, p: f64) -> u64 {
        if self.0.is_empty() {
            return 0;
        }
        let mut sorted = self.0.clone();
        sorted.sort_unstable();
        let rank = ((sorted.len() as f64) * p).ceil() as usize;
        sorted[rank.clamp(1, sorted.len()) - 1]
    }
}

/// Metric name to value. Latencies are microseconds. Names that start with
/// `viol.` are correctness counts and must be 0.
#[derive(Default)]
pub struct Metrics(pub BTreeMap<String, f64>);

impl Metrics {
    pub fn set(&mut self, name: &str, value: f64) {
        self.0.insert(name.to_string(), value);
    }

    pub fn count(&mut self, name: &str, value: u64) {
        self.set(name, value as f64);
    }

    pub fn hist(&mut self, name: &str, hist: &Hist) {
        self.count(&format!("{name}.n"), hist.0.len() as u64);
        self.set(&format!("{name}.p50"), hist.q(0.50) as f64 / 1e3);
        self.set(&format!("{name}.p99"), hist.q(0.99) as f64 / 1e3);
        self.set(&format!("{name}.max"), hist.q(1.0) as f64 / 1e3);
    }

    pub fn json(&self) -> String {
        let body: Vec<String> = self
            .0
            .iter()
            .map(|(name, value)| {
                let value = if value.is_finite() {
                    format!("{value}")
                } else {
                    "null".into()
                };
                format!("\"{name}\":{value}")
            })
            .collect();
        format!("{{{}}}", body.join(","))
    }
}

pub fn rss_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    pages * 4096
}

/// CPU seconds this process has used, user plus system.
pub fn self_cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after = stat.rsplit_once(')').map_or("", |(_, rest)| rest);
    let fields: Vec<&str> = after.split_whitespace().collect();
    let ticks = |i: usize| {
        fields
            .get(i)
            .and_then(|f| f.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    (ticks(11) + ticks(12)) / 100.0
}

fn machine_busy_jiffies() -> f64 {
    let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let line = stat.lines().next().unwrap_or("");
    let v: Vec<f64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    // user nice system idle iowait irq softirq steal
    v.iter()
        .enumerate()
        .filter(|(i, _)| *i != 3 && *i != 4 && *i < 8)
        .map(|(_, x)| x)
        .sum()
}

/// Cores that other processes kept busy over a 500 ms sample taken now.
pub fn other_cores_busy() -> f64 {
    let (m0, s0, t0) = (machine_busy_jiffies(), self_cpu_seconds(), Instant::now());
    std::thread::sleep(Duration::from_millis(500));
    let dt = t0.elapsed().as_secs_f64();
    let machine = (machine_busy_jiffies() - m0) / 100.0;
    ((machine - (self_cpu_seconds() - s0)) / dt).max(0.0)
}

/// Evaluations of the closures that scenarios choose to count. Kept off the
/// tick path: one shared counter there would dominate a 0.1 µs fire.
pub static EVALS: AtomicU64 = AtomicU64::new(0);

pub fn evals() -> u64 {
    EVALS.load(Ordering::Relaxed)
}

pub fn count_eval() {
    EVALS.fetch_add(1, Ordering::Relaxed);
}
