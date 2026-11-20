//! Drivers: a paced trigger with per-tick completion, and a paced probe.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

use super::{
    adapter,
    universe::Acct,
    util::{Hist, Metrics, Rng, now, sleep_until, wait_for},
};

pub const HZ30: u64 = 33_333_333;

/// The phase a scenario is in. Drivers tag each sample with it.
pub static PHASE: AtomicUsize = AtomicUsize::new(0);

pub fn set_phase(phase: usize) {
    PHASE.store(phase, Relaxed);
}

pub struct TickLog {
    /// Admission to last sink per tick in ns, `None` when a sink never saw it.
    pub comp: Vec<Option<u64>>,
    pub phase: Vec<usize>,
    /// Sinks reached per tick.
    pub delivered: Vec<u32>,
    /// The most earlier ticks still incomplete when a tick was admitted.
    pub max_depth: u64,
    /// The same count when the last tick was admitted.
    pub end_depth: u64,
}

/// Fire ticks `first..first + n` at `period` and wait for the last.
pub fn drive(acct: &Acct, first: u64, n: u64, period: u64, fire: impl Fn(u64)) -> TickLog {
    drive_until(acct, first, n, period, &AtomicBool::new(false), fire)
}

/// [`drive`] that also ends when `stop` is set.
pub fn drive_until(
    acct: &Acct,
    first: u64,
    n: u64,
    period: u64,
    stop: &AtomicBool,
    fire: impl Fn(u64),
) -> TickLog {
    let base = now() + 1_000_000;
    let mut cursor = first;
    let (mut max_depth, mut end_depth) = (0, 0);
    let mut phase = Vec::with_capacity(n as usize);
    let mut n = n;
    for i in 0..n {
        let t = first + i;
        sleep_until(base + i * period);
        if stop.load(Relaxed) {
            n = i;
            break;
        }
        while cursor < t && acct.complete(cursor) {
            cursor += 1;
        }
        end_depth = t - cursor;
        max_depth = max_depth.max(end_depth);
        phase.push(PHASE.load(Relaxed));
        acct.start(t, now());
        fire(t);
    }
    wait_for(10_000_000_000, || n == 0 || acct.complete(first + n - 1));
    adapter::flush();
    TickLog {
        comp: (first..first + n).map(|t| acct.completion(t)).collect(),
        delivered: (first..first + n).map(|t| acct.delivered(t)).collect(),
        phase,
        max_depth,
        end_depth,
    }
}

impl TickLog {
    pub fn hist(&self, skip: usize, phase: Option<usize>) -> Hist {
        let mut hist = Hist::default();
        for (i, comp) in self.comp.iter().enumerate().skip(skip) {
            if phase.is_none_or(|p| p == self.phase[i])
                && let Some(ns) = comp
            {
                hist.add(*ns);
            }
        }
        hist
    }

    pub fn incomplete(&self) -> u64 {
        self.comp.iter().filter(|c| c.is_none()).count() as u64
    }

    /// The standard tick metrics under `name`, skipping `warm` ticks.
    pub fn report(&self, m: &mut Metrics, name: &str, warm: usize, acct: &Acct) {
        m.hist(name, &self.hist(warm, None));
        m.count(&format!("{name}.max_depth"), self.max_depth);
        m.count(&format!("{name}.end_depth"), self.end_depth);
        m.count(&format!("viol.{name}.incomplete"), self.incomplete());
        m.count(&format!("viol.{name}.gaps"), acct.gaps.load(Relaxed));
        m.count(&format!("viol.{name}.reorder"), acct.reorder.load(Relaxed));
        m.count(&format!("viol.{name}.dup"), acct.dup.load(Relaxed));
    }
}

#[derive(Default)]
pub struct ProbeLog {
    pub lat: Vec<(usize, u64)>,
    pub lost: u64,
}

/// Call `fire(seq)` about every `period` ns, with jitter so it never locks
/// to the tick, until `stop`. `fire` returns the latency or `None` if lost.
pub fn probe_loop(
    stop: &AtomicBool,
    period: u64,
    seed: u64,
    mut fire: impl FnMut(u64) -> Option<u64>,
) -> ProbeLog {
    let mut rng = Rng::new(seed);
    let mut log = ProbeLog::default();
    let mut next = now();
    let mut seq = 0;
    while !stop.load(Relaxed) {
        next += period / 2 + rng.next() % period;
        sleep_until(next);
        seq += 1;
        let phase = PHASE.load(Relaxed);
        match fire(seq) {
            Some(ns) => log.lat.push((phase, ns)),
            None => log.lost += 1,
        }
        next = next.max(now());
    }
    log
}

impl ProbeLog {
    pub fn hist(&self, phase: usize) -> Hist {
        Hist(
            self.lat
                .iter()
                .filter(|(p, _)| *p == phase)
                .map(|(_, ns)| *ns)
                .collect(),
        )
    }

    /// One histogram per named phase, and the lost count.
    pub fn report(&self, m: &mut Metrics, name: &str, phases: &[&str]) {
        for (phase, label) in phases.iter().enumerate() {
            m.hist(&format!("{name}.{label}"), &self.hist(phase));
        }
        m.count(&format!("viol.{name}.lost"), self.lost);
    }
}
