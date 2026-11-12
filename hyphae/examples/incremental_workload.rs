//! Generic sparse-output workload, not an application integration benchmark.
//! All modes use identical inputs, output sinks and checked final values.
//! Setup and warmup are excluded. There are no sleeps or target update rates.

use std::{hint::black_box, sync::Arc, time::Instant};

use hyphae::{
    Cell, CellImmutable, CellMutable, MapExt, Materialize, Mutable, Signal, SubscriptionGuard,
    SwitchMapExt, Watchable, batch, incremental as inc, local,
};
use parking_lot::Mutex;

const GROUPS: usize = 25;
const OUTPUTS: usize = 27;
const CHANGING: usize = 4;
const DEPTH: u64 = 6;
const WARMUP: u64 = 100;
const UPDATES: u64 = 2000;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type Sink = Arc<Mutex<Output>>;

struct Output {
    values: Vec<u64>,
    deliveries: u64,
    counts: Vec<u64>,
}
fn sink() -> Sink {
    Arc::new(Mutex::new(Output {
        values: vec![0; GROUPS.saturating_mul(OUTPUTS)],
        deliveries: 0,
        counts: vec![0; GROUPS.saturating_mul(OUTPUTS)],
    }))
}
fn record(sink: &Sink, index: usize, value: u64) {
    let mut sink = sink.lock();
    if let Some(slot) = sink.values.get_mut(index) {
        *slot = value;
    }
    sink.deliveries = sink.deliveries.saturating_add(1);
    if let Some(count) = sink.counts.get_mut(index) {
        *count = count.saturating_add(1);
    }
}
const fn stage(value: u64, stage: u64) -> u64 {
    value.wrapping_mul(3).wrapping_add(stage)
}
const fn revision(epoch: u64) -> Option<u64> {
    if epoch.is_multiple_of(31) {
        epoch.checked_div(31)
    } else {
        None
    }
}
fn expected(epoch: u64, index: usize) -> u64 {
    let group = index.checked_div(OUTPUTS).unwrap_or_default();
    let lane = index.checked_rem(OUTPUTS).unwrap_or_default();
    if lane >= CHANGING {
        return u64::try_from(index).unwrap_or_default();
    }
    let mut value = epoch.wrapping_add(u64::try_from(group).unwrap_or_default());
    for depth in 0..DEPTH {
        value = stage(value, depth);
    }
    value.wrapping_add(epoch.checked_div(31).unwrap_or_default())
}

trait Workload {
    fn update(&mut self, epoch: u64) -> Result<()>;
}

struct Shared {
    inputs: Vec<(Cell<u64, CellMutable>, Cell<u64, CellMutable>)>,
    _guards: Vec<SubscriptionGuard>,
    _cells: Vec<Cell<u64, CellImmutable>>,
    batched: bool,
}
impl Shared {
    fn new(output: &Sink, batched: bool) -> Self {
        let mut inputs = Vec::new();
        let mut guards = Vec::new();
        let mut cells = Vec::new();
        for group in 0..GROUPS {
            let input = Cell::new(u64::try_from(group).unwrap_or_default());
            let selector = Cell::new(0_u64);
            for lane in 0..OUTPUTS {
                let index = group.saturating_mul(OUTPUTS).saturating_add(lane);
                let cell = if lane < CHANGING {
                    let mut tail = input.clone().lock();
                    for depth in 0..DEPTH {
                        tail = tail.map(move |v| stage(*v, depth)).materialize();
                    }
                    selector
                        .clone()
                        .switch_map(move |revision| {
                            let revision = *revision;
                            tail.clone()
                                .map(move |v| v.wrapping_add(revision))
                                .materialize()
                        })
                        .materialize()
                } else {
                    Cell::new(u64::try_from(index).unwrap_or_default()).lock()
                };
                let output = output.clone();
                guards.push(cell.subscribe(move |signal| {
                    if let Signal::Value(v) = signal {
                        record(&output, index, **v);
                    }
                }));
                cells.push(cell);
            }
            inputs.push((input, selector));
        }
        Self {
            inputs,
            _guards: guards,
            _cells: cells,
            batched,
        }
    }
}
impl Workload for Shared {
    fn update(&mut self, epoch: u64) -> Result<()> {
        let update = || {
            for (group, (input, selector)) in self.inputs.iter().enumerate() {
                input.set(epoch.wrapping_add(u64::try_from(group).unwrap_or_default()));
                if let Some(value) = revision(epoch) {
                    selector.set(value);
                }
            }
        };
        if self.batched {
            batch(update);
        } else {
            update();
        }
        Ok(())
    }
}

struct Owned {
    rt: local::Runtime,
    inputs: Vec<(local::Input<u64>, local::Input<u64>)>,
    _guards: Vec<local::Observer<u64>>,
}
impl Owned {
    fn new(output: &Sink) -> Result<Self> {
        let mut rt = local::Runtime::new();
        let mut inputs = Vec::new();
        let mut guards = Vec::new();
        for group in 0..GROUPS {
            let input = rt.input(u64::try_from(group).unwrap_or_default())?;
            let selector = rt.input(0_u64)?;
            for lane in 0..OUTPUTS {
                let index = group.saturating_mul(OUTPUTS).saturating_add(lane);
                let cell = if lane < CHANGING {
                    let mut tail = input.cell();
                    for depth in 0..DEPTH {
                        tail = rt.map(&tail, move |v| stage(*v, depth))?;
                    }
                    rt.switch_map(&selector.cell(), move |builder, revision| {
                        let revision = *revision;
                        builder.map(&tail, move |v| v.wrapping_add(revision))
                    })?
                } else {
                    rt.constant(u64::try_from(index).unwrap_or_default())?
                };
                let output = output.clone();
                guards.push(rt.subscribe(&cell, move |v, _| {
                    record(&output, index, *v);
                    Ok(())
                })?);
            }
            inputs.push((input, selector));
        }
        rt.stabilize()?;
        Ok(Self {
            rt,
            inputs,
            _guards: guards,
        })
    }
}
impl Workload for Owned {
    fn update(&mut self, epoch: u64) -> Result<()> {
        self.rt.transaction(|tx| {
            for (group, (input, selector)) in self.inputs.iter().enumerate() {
                tx.set(
                    input,
                    epoch.wrapping_add(u64::try_from(group).unwrap_or_default()),
                )?;
                if let Some(value) = revision(epoch) {
                    tx.set(selector, value)?;
                }
            }
            Ok(())
        })?;
        Ok(())
    }
}

struct Incremental {
    rt: inc::Runtime,
    inputs: Vec<(inc::Input<u64>, inc::Input<u64>)>,
    _guards: Vec<inc::Subscription>,
}
impl Incremental {
    fn new(output: &Sink) -> Result<Self> {
        let mut rt = inc::Runtime::new();
        let mut inputs = Vec::new();
        let mut guards = Vec::new();
        for group in 0..GROUPS {
            let input = rt.input(u64::try_from(group).unwrap_or_default());
            let selector = rt.input(0_u64);
            for lane in 0..OUTPUTS {
                let index = group.saturating_mul(OUTPUTS).saturating_add(lane);
                let cell = if lane < CHANGING {
                    let mut tail = input.cell();
                    for depth in 0..DEPTH {
                        tail = tail.map(move |v| stage(*v, depth));
                    }
                    selector.cell().switch_map(move |_, revision| {
                        let revision = *revision;
                        tail.map(move |v| v.wrapping_add(revision))
                    })
                } else {
                    rt.constant(u64::try_from(index).unwrap_or_default())
                };
                let output = output.clone();
                guards.push(rt.subscribe(&cell, move |v, _| record(&output, index, *v))?);
            }
            inputs.push((input, selector));
        }
        rt.transaction(|_| Ok(()))?;
        Ok(Self {
            rt,
            inputs,
            _guards: guards,
        })
    }
}
impl Workload for Incremental {
    fn update(&mut self, epoch: u64) -> Result<()> {
        self.rt.transaction(|tx| {
            for (group, (input, selector)) in self.inputs.iter().enumerate() {
                tx.set(
                    input,
                    epoch.wrapping_add(u64::try_from(group).unwrap_or_default()),
                )?;
                if let Some(value) = revision(epoch) {
                    tx.set(selector, value)?;
                }
            }
            Ok(())
        })?;
        Ok(())
    }
}

fn measure(name: &str, mut workload: impl Workload, output: &Sink) -> Result<()> {
    for epoch in 1..=WARMUP {
        workload.update(epoch)?;
    }
    {
        let mut output = output.lock();
        output.deliveries = 0;
        output.counts.fill(0);
    }
    let start = Instant::now();
    for epoch in WARMUP.saturating_add(1)..=WARMUP.saturating_add(UPDATES) {
        workload.update(black_box(epoch))?;
    }
    let elapsed = start.elapsed();
    let output = output.lock();
    for (index, actual) in output.values.iter().enumerate() {
        if *actual != expected(WARMUP.saturating_add(UPDATES), index) {
            return Err(format!("{name}: incorrect final output {index}: {actual}").into());
        }
    }
    let edits = WARMUP
        .saturating_add(UPDATES)
        .checked_div(31)
        .unwrap_or_default()
        .saturating_sub(WARMUP.checked_div(31).unwrap_or_default());
    for (index, deliveries) in output.counts.iter().enumerate() {
        let active = index.checked_rem(OUTPUTS).unwrap_or_default() < CHANGING;
        let expected = if active {
            UPDATES.saturating_add(if name == "shared-eager" { edits } else { 0 })
        } else {
            0
        };
        if *deliveries != expected {
            return Err(
                format!("{name}: output {index}: deliveries {deliveries} != {expected}").into(),
            );
        }
    }
    let deliveries = output.deliveries;
    drop(output);
    println!("{name}: {elapsed:?}, deliveries={deliveries}, updates={UPDATES}, final_values=PASS");
    Ok(())
}

fn main() -> Result<()> {
    println!(
        "generic sparse graph: groups={GROUPS}, outputs/group={OUTPUTS}, changing/group={CHANGING}, depth={DEPTH}, edit_every=31; no timers/network/database"
    );
    for _ in 0..3 {
        let output = sink();
        measure("shared-eager", Shared::new(&output, false), &output)?;
        let output = sink();
        measure("shared-batched", Shared::new(&output, true), &output)?;
        let output = sink();
        measure("owned-custom", Owned::new(&output)?, &output)?;
        let output = sink();
        measure("incremental-rs", Incremental::new(&output)?, &output)?;
    }
    Ok(())
}
