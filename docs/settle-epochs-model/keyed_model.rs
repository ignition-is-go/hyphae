// Exhaustive checker for the keyed lookup of hyphae/docs/settle-epochs.md:
// L = C[K], with static edges K -> L and C -> L. A ticket writes the key, an
// entry, or both at once. C routes a change of entry k to L only when L can
// read k at that ticket, using K's watermark to know L's key; when K has not
// passed the ticket, C pushes conservatively.
use std::collections::{HashMap, VecDeque};

const K: usize = 0;
const C: usize = 1;
const L: usize = 2;
const NODES: usize = 3;
const SEED: u8 = 254;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Desc {
    Key(u8),
    Entry(u8, i32),
    Both(u8, i32), // key := k and C[k] := v in one event set
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct St {
    seeded: u8,
    pend: [Vec<(u8, u8)>; NODES],
    key_hist: Vec<(u8, u8)>,
    coll_hist: Vec<(u8, [i32; 2])>,
    out_hist: Vec<(u8, i32)>,
    processed: [i16; NODES],
    cw: [u8; NODES],
    complete: u32,
}

fn mutant(name: &str) -> bool {
    std::env::var("MUTANT").is_ok_and(|m| m == name)
}

fn key_at(s: &St, t: u8) -> u8 {
    s.key_hist.iter().rev().find(|h| h.0 <= t).map_or(0, |h| h.1)
}

fn coll_at(s: &St, t: u8) -> [i32; 2] {
    s.coll_hist.iter().rev().find(|h| h.0 <= t).map_or([0, 0], |h| h.1)
}

fn input_w(s: &St, q: &[Desc], n: usize) -> u8 {
    let _ = q;
    match n {
        L => s.seeded.min(s.cw[K]).min(s.cw[C]),
        _ => s.seeded,
    }
}

fn min_pend(s: &St, n: usize) -> u8 {
    s.pend[n].first().map_or(u8::MAX, |p| p.0)
}

fn push(s: &mut St, t: u8, src: u8, dst: usize) -> Result<(), String> {
    if s.processed[dst] >= t as i16 || s.cw[dst] > t || s.complete & (1 << t) != 0 {
        return Err(format!("unsound push of t{t} to node {dst}"));
    }
    if !s.pend[dst].contains(&(t, src)) {
        s.pend[dst].push((t, src));
        s.pend[dst].sort();
    }
    Ok(())
}

fn succ(s: &St, q: &[Desc], out: &mut Vec<St>) -> Result<(), String> {
    if (s.seeded as usize) < q.len() {
        let mut n = s.clone();
        let t = n.seeded;
        n.seeded += 1;
        match q[t as usize] {
            Desc::Key(_) => push(&mut n, t, SEED, K)?,
            Desc::Entry(..) => push(&mut n, t, SEED, C)?,
            Desc::Both(..) => {
                push(&mut n, t, SEED, K)?;
                push(&mut n, t, SEED, C)?;
            }
        }
        out.push(n);
    }
    for v in 0..NODES {
        let w = input_w(s, q, v).min(min_pend(s, v));
        if w > s.cw[v] {
            let mut n = s.clone();
            n.cw[v] = w;
            out.push(n);
        }
    }
    for v in 0..NODES {
        let Some(&(t, _)) = s.pend[v].first() else { continue };
        if t >= input_w(s, q, v) {
            continue;
        }
        let mut n = s.clone();
        if n.processed[v] >= t as i16 {
            return Err(format!("node {v} fired t{t} out of order"));
        }
        n.processed[v] = t as i16;
        n.pend[v].retain(|p| p.0 != t);
        match v {
            K => {
                let (Desc::Key(k) | Desc::Both(k, _)) = q[t as usize] else { unreachable!() };
                n.key_hist.push((t, k));
                push(&mut n, t, K as u8, L)?;
            }
            C => {
                let (Desc::Entry(k, val) | Desc::Both(k, val)) = q[t as usize] else { unreachable!() };
                let mut c = coll_at(&n, t);
                c[k as usize] = val;
                n.coll_hist.push((t, c));
                // Per-key routing. L's key at t is known once K has passed t.
                let route = if mutant("route_by_last_fired_key") {
                    key_at(&n, n.processed[L].max(0) as u8) == k
                } else if n.cw[K] > t {
                    key_at(&n, t) == k
                } else {
                    true
                };
                if route || mutant("broadcast") {
                    push(&mut n, t, C as u8, L)?;
                }
            }
            L => {
                let key = key_at(&n, t) as usize;
                let now = coll_at(&n, t)[key];
                let prev_key = if t == 0 { 0 } else { key_at(&n, t - 1) as usize };
                let prev = if t == 0 { 0 } else { coll_at(&n, t - 1)[prev_key] };
                let key_changed = n.key_hist.iter().any(|h| h.0 == t);
                let entry_changed = n.coll_hist.iter().any(|h| h.0 == t) && (now != prev || key != prev_key);
                if key_changed || entry_changed || mutant("emit_always") {
                    n.out_hist.push((t, now));
                }
            }
            _ => unreachable!(),
        }
        out.push(n);
    }
    Ok(())
}

/// The output L must equal C[K] after every ticket it emits on, and it must
/// emit exactly on the tickets where the key changed or the entry under the
/// current key changed.
fn serial(q: &[Desc]) -> Vec<(u8, i32)> {
    let (mut key, mut coll) = (0u8, [0i32; 2]);
    let mut out = Vec::new();
    for (t, d) in q.iter().enumerate() {
        let before = coll[key as usize];
        let old_key = key;
        let mut key_changed = false;
        let mut entry_touched = false;
        match *d {
            Desc::Key(k) => {
                key = k;
                key_changed = true;
            }
            Desc::Entry(k, v) => {
                coll[k as usize] = v;
                entry_touched = true;
            }
            Desc::Both(k, v) => {
                key = k;
                coll[k as usize] = v;
                key_changed = true;
                entry_touched = true;
            }
        }
        let now = coll[key as usize];
        if key_changed || (entry_touched && (now != before || key != old_key)) {
            out.push((t as u8, now));
        }
    }
    out
}

fn check(q: &[Desc]) -> Result<usize, String> {
    let init = St {
        seeded: 0,
        pend: Default::default(),
        key_hist: Vec::new(),
        coll_hist: Vec::new(),
        out_hist: Vec::new(),
        processed: [-1; NODES],
        cw: [0; NODES],
        complete: 0,
    };
    let mut ids: HashMap<St, usize> = HashMap::new();
    let mut succs: Vec<Vec<usize>> = vec![Vec::new()];
    let mut work = VecDeque::from([init.clone()]);
    ids.insert(init, 0);
    let mut buf = Vec::new();
    let want = serial(q);
    while let Some(s) = work.pop_front() {
        let id = ids[&s];
        buf.clear();
        succ(&s, q, &mut buf).map_err(|e| format!("{e}\n{s:?}"))?;
        if buf.is_empty() {
            if (s.seeded as usize) != q.len() || s.pend.iter().any(|p| !p.is_empty()) {
                return Err(format!("deadlock {s:?}"));
            }
            if s.out_hist != want {
                return Err(format!("not serializable: got {:?} want {:?}", s.out_hist, want));
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
    let mut q2: Vec<usize> = (0..succs.len()).filter(|&i| indeg[i] == 0).collect();
    let mut seen = 0;
    while let Some(i) = q2.pop() {
        seen += 1;
        for &n in &succs[i] {
            indeg[n] -= 1;
            if indeg[n] == 0 {
                q2.push(n);
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
    let choices = |i: u32, t: u32| -> Desc {
        let v = t as i32 + 1;
        match i {
            0 => Desc::Key(0),
            1 => Desc::Key(1),
            2 => Desc::Entry(0, v),
            3 => Desc::Entry(1, v),
            4 => Desc::Both(0, v),
            _ => Desc::Both(1, v),
        }
    };
    let (mut total, mut configs) = (0, 0);
    for mask in 0..6u32.pow(k) {
        let q: Vec<Desc> = (0..k).map(|t| choices(mask / 6u32.pow(t) % 6, t)).collect();
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
