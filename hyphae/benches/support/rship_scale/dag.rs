//! Reconvergent graphs whose values say which writes they reflect.
//!
//! A region has eight inputs in four pairs. One writer owns a pair and sets
//! both cells to the same sequence number in one write. Every derived value
//! carries, per input, the sequence number it reflects. A join that sees one
//! input at two sequence numbers through two paths marks the value: that is
//! a glitch. A sink that sees the two cells of a pair at different sequence
//! numbers saw half of a write. A sink that sees an input go backwards saw a
//! writer's writes out of order. No reference evaluator is needed: these
//! programs only route their inputs, so an observation equals the program
//! applied to a cut exactly when those three checks pass.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
};

use hyphae::{
    Cell, CellImmutable, CellMap, CellMutable, JoinExt, MapDiff, MapExt, MapQuery, MapValuesExt,
    Materialize, Mutable, Signal, Watchable,
};
use parking_lot::Mutex;

use super::{
    adapter,
    universe::{Keep, Src},
    util::{Rng, now},
};

pub const INPUTS: usize = 8;
pub const PAIRS: usize = 4;
pub const CELLS_PER_REGION: usize = 22;
pub const SINKS_PER_REGION: usize = 4;
pub const MAP_KEYS: u32 = 512;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Vc {
    pub s: [u32; INPUTS],
    pub t0: u64,
    pub glitch: bool,
}

impl Vc {
    fn input(i: usize, seq: u32, t0: u64) -> Self {
        let mut s = [0; INPUTS];
        s[i] = seq;
        Self {
            s,
            t0,
            glitch: false,
        }
    }

    fn merge(a: &Self, b: &Self) -> Self {
        let mut out = Self {
            s: [0; INPUTS],
            t0: a.t0.max(b.t0),
            glitch: a.glitch || b.glitch,
        };
        for i in 0..INPUTS {
            if a.s[i] != 0 && b.s[i] != 0 && a.s[i] != b.s[i] {
                out.glitch = true;
            }
            out.s[i] = a.s[i].max(b.s[i]);
        }
        out
    }
}

type Node = Cell<Vc, CellImmutable>;

pub struct DagSink {
    pub deps: u8,
    pub last: Mutex<[u32; INPUTS]>,
    pub fires: AtomicU64,
    /// Rolling hash of every observation, in order.
    pub hash: AtomicU64,
    pub lat: Mutex<Vec<u64>>,
}

#[derive(Default)]
pub struct DagViol {
    pub glitch: AtomicU64,
    pub torn: AtomicU64,
    pub reorder: AtomicU64,
}

pub struct Region {
    pub inputs: Vec<Src<Vc>>,
    pub sinks: Vec<Arc<DagSink>>,
}

/// A keyed input, a `map_values` view of it, and a mirror rebuilt from the
/// view's diffs alone.
pub struct MapCheck {
    pub input: CellMap<u32, u64, CellMutable>,
    pub mirror: Arc<Mutex<HashMap<u32, u64>>>,
}

pub struct Dag {
    pub regions: Vec<Region>,
    pub maps: Vec<MapCheck>,
    pub viol: Arc<DagViol>,
    pub map_viol: Arc<AtomicU64>,
    keep: Keep,
}

fn join(a: &Node, b: &Node) -> Node {
    a.clone()
        .join(b.clone())
        .map(|v| Vc::merge(&v.0, &v.1))
        .materialize()
}

fn apply(mirror: &mut HashMap<u32, u64>, diff: &MapDiff<u32, u64>, viol: &AtomicU64) {
    match diff {
        MapDiff::Initial { entries } => {
            *mirror = entries.iter().copied().collect();
        }
        MapDiff::Insert { key, value } => {
            if mirror.insert(*key, *value).is_some() {
                viol.fetch_add(1, Relaxed);
            }
        }
        MapDiff::Remove { key, old_value } => {
            if mirror.remove(key) != Some(*old_value) {
                viol.fetch_add(1, Relaxed);
            }
        }
        MapDiff::Update {
            key,
            old_value,
            new_value,
        } => {
            if mirror.insert(*key, *new_value) != Some(*old_value) {
                viol.fetch_add(1, Relaxed);
            }
        }
        MapDiff::Batch { changes } => {
            for change in changes {
                apply(mirror, change, viol);
            }
        }
    }
}

impl Dag {
    /// Even regions are wired as nested diamonds. Odd regions are wired at
    /// random from `seed`, with joins across unequal heights.
    pub fn build(regions: usize, seed: u64) -> Self {
        let viol = Arc::new(DagViol::default());
        let map_viol = Arc::new(AtomicU64::new(0));
        let mut keep = Keep::new();
        let mut rng = Rng::new(seed);
        let regions = (0..regions)
            .map(|r| {
                let random = r % 2 == 1;
                let inputs: Vec<Src<Vc>> =
                    (0..INPUTS).map(|i| Cell::new(Vc::input(i, 1, 0))).collect();
                let l1: Vec<Node> = inputs
                    .iter()
                    .map(|c| c.clone().map(|v| *v).materialize())
                    .collect();
                let mut pick = |n: usize, fixed: usize| if random { rng.below(n) } else { fixed };
                let l2: Vec<Node> = (0..8)
                    .map(|j| {
                        let (a, b) = (pick(8, j), pick(8, (j + 1) % 8));
                        join(&l1[a], &l1[b])
                    })
                    .collect();
                let l3: Vec<Node> = (0..4)
                    .map(|k| {
                        let (a, b) = (pick(8, 2 * k), pick(8, 2 * k + 1));
                        join(&l2[a], &l2[b])
                    })
                    .collect();
                let l4: Vec<Node> = (0..2)
                    .map(|k| {
                        let a = pick(4, 2 * k);
                        let b = &if random {
                            l2[pick(8, 0)].clone()
                        } else {
                            l3[2 * k + 1].clone()
                        };
                        join(&l3[a], b)
                    })
                    .collect();
                let sinks = [&l4[0], &l4[1], &l2[0], &l2[5]]
                    .into_iter()
                    .map(|node| {
                        let deps = hyphae::Gettable::get(node)
                            .s
                            .iter()
                            .enumerate()
                            .fold(0u8, |m, (i, s)| if *s != 0 { m | (1 << i) } else { m });
                        let sink = Arc::new(DagSink {
                            deps,
                            last: Mutex::new([0; INPUTS]),
                            fires: AtomicU64::new(0),
                            hash: AtomicU64::new(0),
                            lat: Mutex::new(Vec::new()),
                        });
                        let (seen, viol) = (Arc::clone(&sink), Arc::clone(&viol));
                        keep.push(Box::new(node.subscribe(move |signal| {
                            let Signal::Value(v) = signal else { return };
                            if v.t0 == 0 {
                                return;
                            }
                            let at = now();
                            if v.glitch {
                                viol.glitch.fetch_add(1, Relaxed);
                            }
                            for w in 0..PAIRS {
                                let (a, b) = (v.s[2 * w], v.s[2 * w + 1]);
                                if a != 0 && b != 0 && a != b {
                                    viol.torn.fetch_add(1, Relaxed);
                                }
                            }
                            let mut last = seen.last.lock();
                            if (0..INPUTS).any(|i| v.s[i] < last[i]) {
                                viol.reorder.fetch_add(1, Relaxed);
                            }
                            *last = v.s;
                            let mut hash = seen.hash.load(Relaxed);
                            for s in v.s {
                                hash = hash.wrapping_mul(0x100_0000_01B3) ^ u64::from(s);
                            }
                            seen.hash.store(hash, Relaxed);
                            drop(last);
                            seen.fires.fetch_add(1, Relaxed);
                            seen.lat.lock().push(at.saturating_sub(v.t0));
                        })));
                        sink
                    })
                    .collect();
                keep.push(Box::new((l1, l2, l3, l4)));
                Region { inputs, sinks }
            })
            .collect();

        let maps = (0..8)
            .map(|_| {
                let input = CellMap::<u32, u64>::new();
                let view = input.clone().map_values(|_, v| v + 1).materialize();
                let mirror = Arc::new(Mutex::new(HashMap::new()));
                let (seen, viol) = (Arc::clone(&mirror), Arc::clone(&map_viol));
                keep.push(Box::new(view.subscribe_diffs(move |diff| {
                    apply(&mut seen.lock(), diff, &viol);
                })));
                keep.push(Box::new(view));
                MapCheck { input, mirror }
            })
            .collect();
        Self {
            regions,
            maps,
            viol,
            map_viol,
            keep,
        }
    }

    /// One write: both cells of `pair` in `region` take `seq`.
    pub fn write(&self, key: u64, region: usize, pair: usize, seq: u32, wait: bool) {
        let inputs = &self.regions[region].inputs;
        let t0 = now();
        let set = || {
            inputs[2 * pair].set(Vc::input(2 * pair, seq, t0));
            inputs[2 * pair + 1].set(Vc::input(2 * pair + 1, seq, t0));
        };
        if wait {
            adapter::write(key, set)
        } else {
            adapter::post(key, set)
        }
    }

    /// Sinks whose last observation is not the final inputs. `finals[r][p]`
    /// is the last sequence number written to pair `p` of region `r`.
    pub fn unconverged(&self, finals: &[[u32; PAIRS]]) -> u64 {
        let mut bad = 0;
        for (region, last_written) in self.regions.iter().zip(finals) {
            for sink in &region.sinks {
                let last = sink.last.lock();
                for i in 0..INPUTS {
                    let wrote = last_written[i / 2];
                    if sink.deps & (1 << i) != 0 && wrote > 1 && last[i] != wrote {
                        bad += 1;
                        break;
                    }
                }
            }
        }
        bad
    }

    /// Map views whose mirror, rebuilt from diffs, differs from the view of
    /// the final input.
    pub fn maps_diverged(&self) -> u64 {
        self.maps
            .iter()
            .filter(|check| {
                let want: HashMap<u32, u64> = check
                    .input
                    .snapshot()
                    .into_iter()
                    .map(|(k, v)| (k, v + 1))
                    .collect();
                *check.mirror.lock() != want
            })
            .count() as u64
    }

    pub fn keep_len(&self) -> usize {
        self.keep.len()
    }
}
