//! The rship-shaped graph every scenario runs inside.
//!
//! Sizes at scale 1 follow the e8 load-test census: about 250,000 live
//! cells, 34 timer sources fanning out to about 25,000 fires per tick, 3,600
//! sink subscribers on the tick, chains 200 levels deep and 40 wide, and one
//! keyed store whose cone is more than half the graph.

use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering::Relaxed},
    },
};

use hyphae::{
    Cell, CellImmutable, CellMap, CellMutable, JoinExt, MapExt, Materialize, Mutable, Signal,
    SubscriptionGuard, Watchable,
};
use parking_lot::Mutex;

use super::{
    adapter::{self, Table},
    dag::Dag,
    util::{count_eval, now, wait_for},
};

pub type Keep = Vec<Box<dyn Any + Send + Sync>>;
/// `(sequence, time the write was admitted)`.
pub type Sv = (u64, u64);
pub type Src<T> = Cell<T, CellMutable>;

pub const TIMER_SOURCES: usize = 34;
pub const TABLE_ROWS: u32 = 256;
pub const DEEP_WIDTH: usize = 40;
pub const DEEP_LEVELS: usize = 200;
pub const ISLAND_CHAINS: usize = 8;
pub const ISLAND_DEPTH: usize = 8;
pub const MAX_TICKS: usize = 16_384;

pub const KEY_TICK: u64 = 1;
pub const KEY_PROBE: u64 = 2;
pub const KEY_STORE: u64 = 3;
pub const KEY_FAN: u64 = 4;
pub const KEY_FEEDBACK: u64 = 5;
pub const KEY_EDIT: u64 = 6;
pub const KEY_WRITER: u64 = 1000;

#[derive(Clone, Copy, Debug)]
pub struct Sizes {
    pub streams: usize,
    pub store_keys: usize,
    pub deep_groups: usize,
    pub regions: usize,
    pub islands: usize,
    pub fan: usize,
    pub feedback: usize,
}

impl Sizes {
    /// `scale` multiplies everything. `bg` multiplies only the state the tick
    /// does not touch (S5). `shrink` divides everything, for a version that
    /// cannot build scale 1.
    pub fn new(scale: usize, bg: f64, shrink: f64) -> Self {
        let front = |n: usize| ((n * scale) as f64 / shrink) as usize;
        let back = |n: usize| ((n * scale) as f64 * bg / shrink) as usize;
        Self {
            streams: front(3_600),
            store_keys: back(45_000),
            deep_groups: back(1).max(1),
            regions: back(900),
            islands: back(480).max(16),
            fan: front(5_000),
            feedback: front(2_000),
        }
    }
}

/// A sink that expects sequence numbers 1, 2, 3, … and records what it saw.
#[derive(Default)]
pub struct SeqSink {
    pub seq: AtomicU64,
    pub ns: AtomicU64,
    pub fires: AtomicU64,
    pub gaps: AtomicU64,
    pub reorder: AtomicU64,
    pub lat: Option<Mutex<Vec<u64>>>,
}

impl SeqSink {
    pub fn new(record: bool) -> Arc<Self> {
        Arc::new(Self {
            lat: record.then(|| Mutex::new(Vec::new())),
            ..Self::default()
        })
    }

    pub fn hit(&self, seq: u64, t0: u64) {
        if seq == 0 {
            return;
        }
        let at = now();
        self.ns.store(at, Relaxed);
        let prev = self.seq.swap(seq, Relaxed);
        if seq > prev + 1 {
            self.gaps.fetch_add(seq - prev - 1, Relaxed);
        } else if seq <= prev {
            self.reorder.fetch_add(1, Relaxed);
        }
        self.fires.fetch_add(1, Relaxed);
        if let Some(lat) = &self.lat {
            lat.lock().push(at.saturating_sub(t0));
        }
    }

    pub fn take_lat(&self) -> Vec<u64> {
        self.lat
            .as_ref()
            .map(|lat| std::mem::take(&mut *lat.lock()))
            .unwrap_or_default()
    }

    pub fn watch(self: &Arc<Self>, cell: &Cell<Sv, CellImmutable>) -> SubscriptionGuard {
        let sink = Arc::clone(self);
        cell.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                sink.hit(v.0, v.1);
            }
        })
    }
}

/// Per-tick and per-stream accounting for a trigger that fans out to
/// `streams` sinks.
pub struct Acct {
    pub streams: u32,
    last_t: Vec<AtomicU64>,
    p_seen: Vec<AtomicU64>,
    p_ns: Vec<AtomicU64>,
    r_seen: Vec<AtomicU64>,
    r_ns: Vec<AtomicU64>,
    done: Vec<AtomicU32>,
    last_ns: Vec<AtomicU64>,
    start_ns: Vec<AtomicU64>,
    pub gaps: AtomicU64,
    pub reorder: AtomicU64,
    pub dup: AtomicU64,
}

const UNSEEN: u64 = u64::MAX;

fn atoms(n: usize, v: u64) -> Vec<AtomicU64> {
    (0..n).map(|_| AtomicU64::new(v)).collect()
}

impl Acct {
    pub fn new(streams: usize) -> Arc<Self> {
        Arc::new(Self {
            streams: streams as u32,
            last_t: atoms(streams, UNSEEN),
            p_seen: atoms(streams, 0),
            p_ns: atoms(streams, 0),
            r_seen: atoms(streams, UNSEEN),
            r_ns: atoms(streams, 0),
            done: (0..MAX_TICKS).map(|_| AtomicU32::new(0)).collect(),
            last_ns: atoms(MAX_TICKS, 0),
            start_ns: atoms(MAX_TICKS, 0),
            gaps: AtomicU64::new(0),
            reorder: AtomicU64::new(0),
            dup: AtomicU64::new(0),
        })
    }

    pub fn sink(&self, i: usize, t: u64, p: u64, r: u64) {
        let at = now();
        let prev = self.last_t[i].swap(t, Relaxed);
        let mut changed = false;
        if prev != UNSEEN && t != prev {
            if t > prev + 1 {
                self.gaps.fetch_add(t - prev - 1, Relaxed);
            } else if t < prev {
                self.reorder.fetch_add(1, Relaxed);
            }
            if let Some(done) = self.done.get(t as usize) {
                self.last_ns[t as usize].fetch_max(at, Relaxed);
                done.fetch_add(1, Relaxed);
            }
            changed = true;
        }
        if self.p_seen[i].load(Relaxed) != p {
            self.p_ns[i].store(at, Relaxed);
            self.p_seen[i].store(p, Relaxed);
            changed = true;
        }
        if self.r_seen[i].load(Relaxed) != r {
            self.r_ns[i].store(at, Relaxed);
            self.r_seen[i].store(r, Relaxed);
            changed = true;
        }
        if !changed && prev != UNSEEN {
            self.dup.fetch_add(1, Relaxed);
        }
    }

    pub fn start(&self, t: u64, at: u64) {
        self.start_ns[t as usize].store(at, Relaxed);
    }

    pub fn complete(&self, t: u64) -> bool {
        self.done[t as usize].load(Relaxed) >= self.streams
    }

    pub fn delivered(&self, t: u64) -> u32 {
        self.done[t as usize].load(Relaxed)
    }

    /// Admission to last sink, or `None` while a sink has not seen `t`.
    pub fn completion(&self, t: u64) -> Option<u64> {
        self.complete(t).then(|| {
            self.last_ns[t as usize]
                .load(Relaxed)
                .saturating_sub(self.start_ns[t as usize].load(Relaxed))
        })
    }

    pub fn last_t(&self, i: usize) -> u64 {
        self.last_t[i].load(Relaxed)
    }

    pub fn param_seen(&self, i: usize) -> (u64, u64) {
        (self.p_seen[i].load(Relaxed), self.p_ns[i].load(Relaxed))
    }

    pub fn row_seen(&self, i: usize) -> (u64, u64) {
        (self.r_seen[i].load(Relaxed), self.r_ns[i].load(Relaxed))
    }
}

/// One tick stream: speed, multiply, add, offset, switch, comp element, sink.
pub fn build_stream(
    src: &Src<u64>,
    param: &Src<u64>,
    key: &Src<u32>,
    table: &Table,
    sink: impl Fn(u64, u64, u64) + Send + Sync + 'static,
) -> Keep {
    let n1 = src.clone().map(|t| t + 1).materialize();
    let n2 = n1
        .clone()
        .join(param.clone())
        .map(|v| (v.0 + 1, v.1))
        .materialize();
    let n3 = n2.clone().map(|v| (v.0 + 1, v.1)).materialize();
    let n4 = n3.clone().map(|v| (v.0 + 1, v.1)).materialize();
    let row = table.lookup(key);
    let n5 = n4
        .clone()
        .join(row.clone())
        .map(|v| (v.0.0 + 1, v.0.1, v.1.unwrap_or(u64::MAX - 1)))
        .materialize();
    let n6 = n5.clone().map(|v| (v.0 + 1, v.1, v.2)).materialize();
    let guard = n6.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            sink(v.0 - 6, v.1, v.2);
        }
    });
    vec![
        Box::new(n1),
        Box::new(n2),
        Box::new(n3),
        Box::new(n4),
        Box::new(row),
        Box::new(n5),
        Box::new(n6),
        Box::new(guard),
    ]
}

pub struct Tick {
    pub sources: Vec<Src<u64>>,
    pub params: Vec<Src<u64>>,
    pub keys: Vec<Src<u32>>,
    pub rows: CellMap<u32, u64, CellMutable>,
    pub table: Table,
    pub acct: Arc<Acct>,
    keep: Keep,
}

pub fn row_value(key: u32) -> u64 {
    u64::from(key) * 10
}

impl Tick {
    pub fn build(streams: usize) -> Self {
        let sources: Vec<_> = (0..TIMER_SOURCES).map(|_| Cell::new(0u64)).collect();
        let rows = CellMap::<u32, u64>::new();
        rows.insert_many((0..TABLE_ROWS).map(|k| (k, row_value(k))).collect());
        let table = Table::new(&rows);
        let acct = Acct::new(streams);
        let mut keep = Keep::new();
        let mut params = Vec::with_capacity(streams);
        let mut keys = Vec::with_capacity(streams);
        for i in 0..streams {
            let param = Cell::new(0u64);
            let key = Cell::new(i as u32 % TABLE_ROWS);
            let acct = Arc::clone(&acct);
            keep.extend(build_stream(
                &sources[i % TIMER_SOURCES],
                &param,
                &key,
                &table,
                move |t, p, r| {
                    acct.sink(i, t, p, r);
                },
            ));
            params.push(param);
            keys.push(key);
        }
        Self {
            sources,
            params,
            keys,
            rows,
            table,
            acct,
            keep,
        }
    }

    /// One tick: 34 timer rows in one write, not waited for, as rship's
    /// timer source submits it.
    pub fn fire(&self, t: u64) {
        adapter::post(KEY_TICK, || {
            for source in &self.sources {
                source.set(t);
            }
        });
    }
}

/// A source, a chain of five, and a sink. Its cone shares no node with
/// anything else.
pub struct Probe {
    pub src: Src<Sv>,
    pub sink: Arc<SeqSink>,
    keep: Keep,
}

pub fn chain(src: &Src<Sv>, levels: usize, keep: &mut Keep) -> Cell<Sv, CellImmutable> {
    let mut cur = src.clone().map(|v| *v).materialize();
    for _ in 1..levels {
        let next = cur.clone().map(|v| *v).materialize();
        keep.push(Box::new(cur));
        cur = next;
    }
    cur
}

impl Probe {
    pub fn build(record: bool) -> Self {
        adapter::events(|| Self::build_inner(record))
    }

    fn build_inner(record: bool) -> Self {
        let src = Cell::new((0u64, 0u64));
        let mut keep = Keep::new();
        let end = chain(&src, 5, &mut keep);
        let sink = SeqSink::new(record);
        keep.push(Box::new(sink.watch(&end)));
        keep.push(Box::new(end));
        Self { src, sink, keep }
    }

    /// Write `seq` and wait for the sink. Admission to sink in ns, or `None`
    /// when the sink never saw it.
    pub fn fire(&self, seq: u64) -> Option<u64> {
        let t0 = now();
        adapter::post(KEY_PROBE, || self.src.set((seq, t0)));
        wait_for(2_000_000_000, || self.sink.seq.load(Relaxed) >= seq)
            .then(|| self.sink.ns.load(Relaxed).saturating_sub(t0))
    }
}

/// The hot source: one keyed store, a three-cell pipeline per key, and a
/// subscriber on every eighth key.
pub struct Store {
    pub map: CellMap<u32, u64, CellMutable>,
    pub sinks: Vec<Arc<SeqSink>>,
    pub sub_every: usize,
    keep: Keep,
}

impl Store {
    pub fn build(keys: usize, interleave: bool) -> Self {
        let map = CellMap::<u32, u64>::new();
        if !interleave {
            map.insert_many((0..keys as u32).map(|k| (k, 0)).collect());
        }
        let sub_every = 8;
        let mut keep = Keep::new();
        let mut sinks = Vec::new();
        for k in 0..keys as u32 {
            if interleave {
                adapter::write(KEY_STORE, || map.insert(k, 0));
            }
            let c1 = map.get(&k).materialize();
            let c2 = c1
                .clone()
                .map(|v| {
                    count_eval();
                    v.unwrap_or(0)
                })
                .materialize();
            let c3 = c2
                .clone()
                .map(|v| {
                    count_eval();
                    *v
                })
                .materialize();
            if (k as usize).is_multiple_of(sub_every) {
                let sink = SeqSink::new(false);
                let seen = Arc::clone(&sink);
                keep.push(Box::new(c3.subscribe(move |signal| {
                    if let Signal::Value(v) = signal {
                        seen.hit(**v, 0);
                    }
                })));
                sinks.push(sink);
            }
            keep.push(Box::new((c1, c2, c3)));
        }
        Self {
            map,
            sinks,
            sub_every,
            keep,
        }
    }

    /// Update the row of subscribed key number `slot` to `seq` and wait for
    /// its sink.
    pub fn fire(&self, slot: usize, seq: u64) -> Option<u64> {
        let key = (slot * self.sub_every) as u32;
        let sink = &self.sinks[slot];
        let t0 = now();
        adapter::post(KEY_STORE, || {
            self.map.insert(key, seq);
        });
        wait_for(2_000_000_000, || sink.seq.load(Relaxed) >= seq)
            .then(|| sink.ns.load(Relaxed).saturating_sub(t0))
    }
}

/// Groups of 40 chains, 200 levels each, with a tap at level 100.
pub struct Deep {
    pub srcs: Vec<Src<Sv>>,
    pub mid: Vec<Arc<SeqSink>>,
    pub end: Vec<Arc<SeqSink>>,
    keep: Keep,
}

impl Deep {
    pub fn build(groups: usize) -> Self {
        let mut deep = Self {
            srcs: Vec::new(),
            mid: Vec::new(),
            end: Vec::new(),
            keep: Keep::new(),
        };
        for _ in 0..groups * DEEP_WIDTH {
            let src = Cell::new((0u64, 0u64));
            let half = chain(&src, DEEP_LEVELS / 2, &mut deep.keep);
            let mid = SeqSink::new(true);
            deep.keep.push(Box::new(mid.watch(&half)));
            let mut cur = half;
            for _ in DEEP_LEVELS / 2..DEEP_LEVELS {
                let next = cur.clone().map(|v| *v).materialize();
                deep.keep.push(Box::new(cur));
                cur = next;
            }
            let end = SeqSink::new(true);
            deep.keep.push(Box::new(end.watch(&cur)));
            deep.keep.push(Box::new(cur));
            deep.srcs.push(src);
            deep.mid.push(mid);
            deep.end.push(end);
        }
        deep
    }
}

/// A writer's private sub-graph: one source, eight chains of eight, eight
/// sinks. The last chain's sink records latency.
pub struct Island {
    pub src: Src<Sv>,
    pub sinks: Vec<Arc<SeqSink>>,
}

pub fn build_islands(n: usize, keep: &mut Keep) -> Vec<Island> {
    (0..n)
        .map(|_| {
            let src = Cell::new((0u64, 0u64));
            let sinks = (0..ISLAND_CHAINS)
                .map(|c| {
                    let end = chain(&src, ISLAND_DEPTH, keep);
                    let sink = SeqSink::new(c == ISLAND_CHAINS - 1);
                    keep.push(Box::new(sink.watch(&end)));
                    keep.push(Box::new(end));
                    sink
                })
                .collect();
            Island { src, sinks }
        })
        .collect()
}

impl Island {
    pub fn last(&self) -> &SeqSink {
        &self.sinks[ISLAND_CHAINS - 1]
    }
}

/// One trigger, `streams` independent chains of five, one sink each.
pub struct Fan {
    pub trigger: Src<Sv>,
    pub acct: Arc<Acct>,
    keep: Keep,
}

impl Fan {
    pub fn build(streams: usize) -> Self {
        let trigger = Cell::new((0u64, 0u64));
        let acct = Acct::new(streams);
        let mut keep = Keep::new();
        for i in 0..streams {
            let end = chain(&trigger, 5, &mut keep);
            let acct = Arc::clone(&acct);
            keep.push(Box::new(end.subscribe(move |signal| {
                if let Signal::Value(v) = signal {
                    acct.sink(i, v.0, 0, UNSEEN);
                }
            })));
            keep.push(Box::new(end));
        }
        Self {
            trigger,
            acct,
            keep,
        }
    }

    pub fn fire(&self, t: u64) {
        adapter::post(KEY_FAN, || self.trigger.set((t, 0)));
    }
}

/// `links` cells in a row. A subscriber on each writes the next.
pub struct Feedback {
    pub head: Src<Sv>,
    pub sink: Arc<SeqSink>,
    pub link_viol: Arc<AtomicU64>,
    keep: Keep,
}

impl Feedback {
    pub fn build(links: usize) -> Self {
        let cells: Vec<Src<Sv>> = (0..=links).map(|_| Cell::new((0u64, 0u64))).collect();
        let link_viol = Arc::new(AtomicU64::new(0));
        let mut keep = Keep::new();
        for i in 0..links {
            let next = cells[i + 1].clone();
            let viol = Arc::clone(&link_viol);
            let last = AtomicU64::new(0);
            keep.push(Box::new(cells[i].subscribe(move |signal| {
                let Signal::Value(v) = signal else { return };
                if v.0 == 0 {
                    return;
                }
                if last.swap(v.0, Relaxed) + 1 != v.0 {
                    viol.fetch_add(1, Relaxed);
                }
                let value = **v;
                adapter::from_callback(|| next.set(value));
            })));
        }
        let sink = SeqSink::new(false);
        let seen = Arc::clone(&sink);
        keep.push(Box::new(cells[links].subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                seen.hit(v.0, v.1);
            }
        })));
        let head = cells[0].clone();
        keep.push(Box::new(cells));
        Self {
            head,
            sink,
            link_viol,
            keep,
        }
    }

    pub fn fire(&self, seq: u64) -> Option<u64> {
        let t0 = now();
        adapter::post(KEY_FEEDBACK, || self.head.set((seq, t0)));
        wait_for(20_000_000_000, || self.sink.seq.load(Relaxed) >= seq)
            .then(|| self.sink.ns.load(Relaxed).saturating_sub(t0))
    }
}

/// What the tick and the probe need. Built first, so the setup scenario can
/// drive it while the rest is built.
pub struct Front {
    pub tick: Tick,
    pub probe: Probe,
}

pub struct Back {
    pub store: Store,
    pub deep: Deep,
    pub islands: Vec<Island>,
    pub fan: Fan,
    pub feedback: Feedback,
    pub dag: Dag,
    keep: Keep,
}

impl Front {
    pub fn build(sizes: &Sizes) -> Self {
        Self {
            tick: Tick::build(sizes.streams),
            probe: Probe::build(false),
        }
    }
}

impl Back {
    pub fn build(sizes: &Sizes, interleave: bool) -> Self {
        let mut keep = Keep::new();
        // The tick, the fan, the store and the reconvergent graphs hold
        // latest values. The rest carry events: every write must reach its
        // sinks. On v3.1.12 the two are different kinds of cell, and an event
        // cell is not glitch-free at a join.
        let store = Store::build(sizes.store_keys, interleave);
        let fan = Fan::build(sizes.fan);
        let dag = Dag::build(sizes.regions, 0x5EED);
        let (deep, islands, feedback) = adapter::events(|| {
            let deep = Deep::build(sizes.deep_groups);
            let islands = build_islands(sizes.islands, &mut keep);
            (deep, islands, Feedback::build(sizes.feedback))
        });
        Self {
            store,
            deep,
            islands,
            fan,
            feedback,
            dag,
            keep,
        }
    }
}

impl Sizes {
    /// Materialized cells the suite builds, counted from the builders.
    pub fn cells(&self) -> usize {
        self.streams * 7
            + self.store_keys * 3
            + self.deep_groups * DEEP_WIDTH * DEEP_LEVELS
            + self.regions * super::dag::CELLS_PER_REGION
            + self.islands * ISLAND_CHAINS * ISLAND_DEPTH
            + self.fan * 5
            + self.feedback
    }

    pub fn subscribers(&self) -> usize {
        self.streams
            + self.store_keys / 8
            + self.deep_groups * DEEP_WIDTH * 2
            + self.regions * super::dag::SINKS_PER_REGION
            + self.islands * ISLAND_CHAINS
            + self.fan
            + self.feedback
    }
}

#[allow(dead_code)]
fn _keeps(front: &Front, back: &Back) -> usize {
    front.tick.keep.len()
        + front.probe.keep.len()
        + back.store.keep.len()
        + back.deep.keep.len()
        + back.fan.keep.len()
        + back.feedback.keep.len()
        + back.keep.len()
}
