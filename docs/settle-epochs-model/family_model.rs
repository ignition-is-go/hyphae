// Exhaustive checker for families (P3) and joins over a reactive key set (P4)
// in hyphae/docs/settle-epochs.md. Store I, demand cell K (a set of keys),
// family node Fam, reader L = sum of Fam's rows for the keys in K. Fam builds
// a row node F_k inside the ticket that first demands k and tears it down
// inside the ticket that drops the last demand.
use std::collections::{HashMap, VecDeque};

const I: usize = 0;
const K: usize = 1;
const FAM: usize = 2;
const L: usize = 3;
const F0: usize = 4;
const NODES: usize = 6;
const INF: u8 = 255;
const SEED: u8 = 254;
const KEYS: usize = 2;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Desc {
    Store(i32),
    Demand(u8), // bitmask of demanded keys
    /// A store write and a demand change in one event set (BOTH=1).
    Both(i32, u8),
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct Edge {
    src: u8,
    dst: u8,
    from: u8,
    to: u8,
    /// `INF`, or the node whose delivery at `t` to `dst` makes this edge
    /// count at `t` (GATE mode).
    gate: u8,
    /// False for an edge that only carries the watermark: `src` pushes
    /// nothing along it.
    data: bool,
}

const fn edge(src: usize, dst: usize, from: u8) -> Edge {
    Edge { src: src as u8, dst: dst as u8, from, to: INF, gate: INF, data: true }
}

/// GATE=1: L reads the rows directly. Fam's edge into L counts at ticket t
/// only when K delivered t to L, and Fam writes L only at such a ticket.
/// The rows feed Fam's watermark only, so Fam tears a row down after it has
/// passed the ticket.
fn gated() -> bool {
    std::env::var("GATE").is_ok_and(|g| g == "1")
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct St {
    seeded: u8,
    pend: [Vec<(u8, u8)>; NODES],
    hist: [Vec<(u8, i32)>; NODES],
    processed: [i16; NODES],
    cw: [u8; NODES],
    edges: Vec<Edge>,
    built: [bool; KEYS],
    diffs: Vec<(u8, Diff)>,
    complete: u32,
}

fn mutant(name: &str) -> bool {
    std::env::var("MUTANT").is_ok_and(|m| m == name)
}

fn at(s: &St, n: usize, t: u8) -> i32 {
    s.hist[n].iter().rev().find(|h| h.0 <= t).map_or(0, |h| h.1)
}

fn contrib(s: &St, e: &Edge) -> u8 {
    let lo = e.from.max(s.cw[e.src as usize]);
    if lo >= e.to { INF } else { lo }
}

/// `in_w(n)` over the edges that always count. A gated edge is left out: it
/// can deliver `t` only after its gate delivered `t`, which holds `n`'s own
/// pending list at `t`.
fn input_w(s: &St, n: usize) -> u8 {
    s.edges
        .iter()
        .filter(|e| e.dst as usize == n && e.gate == INF)
        .map(|e| contrib(s, e))
        .fold(s.seeded, u8::min)
}

/// The fire rule at `t`: every edge that always counts is past `t`, and so is
/// every gated edge whose gate delivered `t`.
fn may_fire(s: &St, n: usize, t: u8) -> bool {
    if t >= input_w(s, n) {
        return false;
    }
    s.edges.iter().filter(|e| e.dst as usize == n && e.gate != INF).all(|e| {
        let gate = if mutant("gate_wrong") { F0 as u8 } else { e.gate };
        let open = !mutant("no_gate") && s.pend[n].contains(&(t, gate));
        !open || contrib(s, e) > t
    })
}

fn min_pend(s: &St, n: usize) -> u8 {
    s.pend[n].first().map_or(INF, |p| p.0)
}

fn push(s: &mut St, t: u8, src: u8, dst: usize) -> Result<(), String> {
    if s.processed[dst] >= t as i16 || s.cw[dst] > t {
        return Err(format!("unsound push of t{t} to node {dst}"));
    }
    if s.complete & (1 << t) != 0 {
        return Err(format!("t{t} reached node {dst} after it completed"));
    }
    if !s.pend[dst].contains(&(t, src)) {
        s.pend[dst].push((t, src));
        s.pend[dst].sort();
    }
    Ok(())
}

/// SHAPE=coll makes a row a collection: a set over two elements, here a
/// bitmask. Otherwise a row is one value.
fn coll() -> bool {
    std::env::var("SHAPE").is_ok_and(|s| s == "coll")
}

fn row(store: i32, k: usize) -> i32 {
    if coll() { (store ^ (k as i32 + 1)) & 3 } else { 10 * store + k as i32 + 1 }
}

fn combine(acc: i32, r: i32) -> i32 {
    if coll() { acc | r } else { acc + r }
}

/// A collection reader's diff: at a key change the snapshot of the new key
/// set's rows, otherwise the elements added and removed.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Diff {
    Initial(i32),
    Delta { added: i32, removed: i32 },
}

fn diff(key_changed: bool, prev: i32, now: i32) -> Diff {
    if key_changed {
        Diff::Initial(now)
    } else {
        Diff::Delta { added: now & !prev, removed: prev & !now }
    }
}

/// Replays a diff stream the way a subscriber holds it: an insert must name an
/// absent element and a remove a present one.
fn replay(diffs: &[(u8, Diff)]) -> Result<Vec<(u8, i32)>, String> {
    let mut held = 0;
    let mut seen = Vec::new();
    for &(t, d) in diffs {
        match d {
            Diff::Initial(v) => held = v,
            Diff::Delta { added, removed } => {
                if held & added != 0 || held & removed != removed {
                    return Err(format!("diff at t{t} does not start from what the subscriber holds"));
                }
                held = (held | added) & !removed;
            }
        }
        seen.push((t, held));
    }
    Ok(seen)
}

fn succ(s: &St, q: &[Desc], out: &mut Vec<St>) -> Result<(), String> {
    if (s.seeded as usize) < q.len() {
        let mut n = s.clone();
        let t = n.seeded;
        n.seeded += 1;
        match q[t as usize] {
            Desc::Store(_) => push(&mut n, t, SEED, I)?,
            Desc::Demand(_) => push(&mut n, t, SEED, K)?,
            Desc::Both(..) => {
                push(&mut n, t, SEED, I)?;
                push(&mut n, t, SEED, K)?;
            }
        }
        out.push(n);
    }
    for v in 0..NODES {
        let w = input_w(s, v).min(min_pend(s, v));
        if w > s.cw[v] {
            let mut n = s.clone();
            n.cw[v] = w;
            out.push(n);
        }
    }
    for v in 0..NODES {
        let Some(&(t, _)) = s.pend[v].first() else { continue };
        if !may_fire(s, v, t) {
            continue;
        }
        // Without the anchor edge Fam still reads the store at t, so it waits
        // for it like a side read.
        if v == FAM && mutant("no_anchor") && s.cw[I] <= t {
            continue;
        }
        let mut n = s.clone();
        if n.processed[v] >= t as i16 {
            return Err(format!("node {v} fired t{t} out of order"));
        }
        n.processed[v] = t as i16;
        n.pend[v].retain(|p| p.0 != t);
        let value = match v {
            I => match q[t as usize] {
                Desc::Store(x) | Desc::Both(x, _) => x,
                Desc::Demand(_) => at(&n, I, t),
            },
            K => match q[t as usize] {
                Desc::Demand(m) | Desc::Both(_, m) => m as i32,
                Desc::Store(_) => at(&n, K, t),
            },
            FAM => {
                let demand = at(&n, K, t) as u8;
                let mut sum = 0;
                for k in 0..KEYS {
                    let f = F0 + k;
                    let wanted = demand & (1 << k) != 0;
                    if wanted && !n.built[k] {
                        // Build inside the ticket: the row's value at t comes
                        // from the store at t; edges start at t+1; tickets the
                        // store already fired above t are backfilled.
                        n.built[k] = true;
                        // A rebuild keeps the torn-down row's history: in GATE
                        // mode L may still read it below t, as a reader in
                        // hyphae still holds deliveries the old row made.
                        n.hist[f].push((t, row(at(&n, I, t), k)));
                        n.processed[f] = t as i16;
                        n.cw[f] = t + 1;
                        n.pend[f].clear();
                        n.edges.push(edge(I, f, t + 1));
                        if gated() {
                            n.edges.push(Edge { data: false, ..edge(f, FAM, t + 1) });
                            n.edges.push(edge(f, L, t + 1));
                        } else {
                            n.edges.push(edge(f, FAM, t + 1));
                        }
                        n.edges.sort();
                        if !mutant("no_backfill") {
                            let back: Vec<u8> = n.hist[I].iter().map(|h| h.0).filter(|&x| x > t).collect();
                            for x in back {
                                push(&mut n, x, I as u8, f)?;
                            }
                        }
                    } else if !wanted && n.built[k] && !mutant("no_teardown") && !mutant("teardown_by_reader") {
                        // Tear down inside the ticket that drops the last
                        // reader. Fam has passed t on this row, so nothing at
                        // or below t is lost.
                        n.built[k] = false;
                        n.edges.retain(|e| e.src as usize != f && e.dst as usize != f);
                        n.pend[f].clear();
                    }
                    if wanted {
                        sum = combine(sum, at(&n, f, t));
                    }
                }
                sum
            }
            L => {
                if mutant("teardown_by_reader") {
                    // The reader drops its refcount when it fires, while the
                    // family may already be past t.
                    let demand = at(&n, K, t) as u8;
                    for k in 0..KEYS {
                        if demand & (1 << k) == 0 && n.built[k] {
                            let f = F0 + k;
                            n.built[k] = false;
                            n.edges.retain(|e| e.src as usize != f && e.dst as usize != f);
                            n.pend[f].clear();
                        }
                    }
                }
                if gated() {
                    let demand = at(&n, K, t) as u8;
                    (0..KEYS)
                        .filter(|k| demand & (1 << k) != 0)
                        .map(|k| at(&n, F0 + k, t))
                        .fold(0, combine)
                } else {
                    at(&n, FAM, t)
                }
            }
            _ => row(at(&n, I, t), v - F0),
        };
        let prev = if t == 0 { 0 } else { at(&n, v, t - 1) };
        let emits = v != L || n.hist[K].iter().any(|h| h.0 == t) || value != prev;
        if emits {
            n.hist[v].push((t, value));
            if v == L && coll() {
                let key_changed = n.hist[K].iter().any(|h| h.0 == t);
                n.diffs.push((t, diff(key_changed, prev, value)));
                if key_changed && mutant("diff_at_switch") {
                    n.diffs.push((t, diff(false, prev, value)));
                }
            }
        }
        let outs: Vec<Edge> = n
            .edges
            .iter()
            .filter(|e| e.src as usize == v && e.data && e.from <= t && t < e.to)
            // Fam writes L only at a ticket that changed L's key.
            .filter(|e| {
                !(gated() && v == FAM && e.dst as usize == L)
                    || mutant("fam_nonkey")
                    || n.hist[K].iter().any(|h| h.0 == t)
            })
            .copied()
            .collect();
        for e in outs {
            push(&mut n, t, v as u8, e.dst as usize)?;
        }
        out.push(n);
    }
    Ok(())
}

/// L after each ticket equals the sum of rows of the demanded keys, and it
/// emits when the demand changed or its value changed.
fn serial(q: &[Desc]) -> Vec<(u8, i32)> {
    let (mut store, mut demand, mut last) = (0, 0u8, 0);
    let mut out = Vec::new();
    for (t, d) in q.iter().enumerate() {
        let demand_changed = matches!(d, Desc::Demand(_) | Desc::Both(..));
        match *d {
            Desc::Store(x) => store = x,
            Desc::Demand(m) => demand = m,
            Desc::Both(x, m) => {
                store = x;
                demand = m;
            }
        }
        let v: i32 = (0..KEYS).filter(|k| demand & (1 << k) != 0).map(|k| row(store, k)).fold(0, combine);
        if demand_changed || v != last {
            out.push((t as u8, v));
            last = v;
        }
    }
    out
}

fn check(q: &[Desc]) -> Result<usize, String> {
    let fam_l = if gated() { Edge { gate: K as u8, ..edge(FAM, L, 0) } } else { edge(FAM, L, 0) };
    let mut edges = vec![edge(K, FAM, 0), fam_l, edge(K, L, 0)];
    if !mutant("no_anchor") {
        edges.push(edge(I, FAM, 0));
    }
    edges.sort();
    let init = St {
        seeded: 0,
        pend: Default::default(),
        hist: Default::default(),
        processed: [-1; NODES],
        cw: [0; NODES],
        edges,
        built: [false; KEYS],
        diffs: Vec::new(),
        complete: 0,
    };
    let want = serial(q);
    let mut ids: HashMap<St, usize> = HashMap::new();
    let mut succs: Vec<Vec<usize>> = vec![Vec::new()];
    let mut work = VecDeque::from([init.clone()]);
    ids.insert(init, 0);
    let mut buf = Vec::new();
    while let Some(s) = work.pop_front() {
        let id = ids[&s];
        buf.clear();
        succ(&s, q, &mut buf).map_err(|e| format!("{e}\n{s:?}"))?;
        if buf.is_empty() {
            if (s.seeded as usize) != q.len() || s.pend.iter().any(|p| !p.is_empty()) {
                return Err(format!("deadlock {s:?}"));
            }
            if s.hist[L] != want {
                return Err(format!("not serializable: L got {:?} want {:?}", s.hist[L], want));
            }
            if coll() {
                let held = replay(&s.diffs)?;
                if held != want {
                    return Err(format!("diff stream holds {held:?}, want {want:?}"));
                }
            }
        }
        for mut n in buf.drain(..) {
            for t in 0..n.seeded {
                if !n.pend.iter().any(|p| p.iter().any(|e| e.0 == t)) {
                    n.complete |= 1 << t;
                }
            }
            let next = ids.len();
            let nid = *ids.entry(n.clone()).or_insert_with(|| {
                work.push_back(n);
                succs.push(Vec::new());
                next
            });
            succs[id].push(nid);
        }
    }
    let mut indeg = vec![0usize; succs.len()];
    succs.iter().flatten().for_each(|&n| indeg[n] += 1);
    let mut stack: Vec<usize> = (0..succs.len()).filter(|&i| indeg[i] == 0).collect();
    let mut seen = 0;
    while let Some(i) = stack.pop() {
        seen += 1;
        for &n in &succs[i] {
            indeg[n] -= 1;
            if indeg[n] == 0 {
                stack.push(n);
            }
        }
    }
    if seen != succs.len() {
        return Err("livelock".into());
    }
    Ok(ids.len())
}

fn main() {
    let k: u32 = std::env::args().nth(1).map_or(4, |a| a.parse().unwrap());
    let (mut total, mut configs) = (0, 0);
    // BOTH=1 adds four letters: a store write with each demand, in one ticket.
    let letters: u32 = if std::env::var("BOTH").is_ok_and(|b| b == "1") { 9 } else { 5 };
    for mask in 0..letters.pow(k) {
        let q: Vec<Desc> = (0..k)
            .map(|t| match mask / letters.pow(t) % letters {
                0 => Desc::Store(t as i32 + 1),
                m @ 1..=4 => Desc::Demand((m - 1) as u8),
                m => Desc::Both(t as i32 + 1, (m - 5) as u8),
            })
            .collect();
        match check(&q) {
            Ok(n) => {
                total += n;
                configs += 1;
            }
            Err(e) => {
                println!("FAIL {q:?}\n{e}");
                std::process::exit(1);
            }
        }
    }
    println!("all ok, {configs} ticket sequences, {total} states");
}
