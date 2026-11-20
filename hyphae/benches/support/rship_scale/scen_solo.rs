//! Scenarios that run without the tick. Each still runs inside the whole
//! graph, with the disjoint probe beside it.

use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    time::Duration,
};

use hyphae::{Cell, MapExt, Materialize, Mutable, Watchable};

use super::{
    Ctx, adapter,
    adapter::Table,
    dag::{Dag, MAP_KEYS, PAIRS},
    load::{ProbeLog, probe_loop, set_phase},
    scen_tick::build,
    universe::{DEEP_LEVELS, DEEP_WIDTH, Front, ISLAND_CHAINS, Island, KEY_WRITER, SeqSink},
    util::{self, Hist, Rng, count_eval, now, sleep_until, wait_for},
};

/// Run `main` with the disjoint probe beside it. Phase 0 is `idle_ms` of
/// probe alone; `main` starts in phase 1.
fn beside_probe<R>(front: &Front, idle_ms: u64, main: impl FnOnce() -> R) -> (ProbeLog, R) {
    let stop = AtomicBool::new(false);
    set_phase(0);
    std::thread::scope(|s| {
        let probe = s.spawn(|| probe_loop(&stop, 5_000_000, 7, |seq| front.probe.fire(seq)));
        std::thread::sleep(Duration::from_millis(idle_ms));
        set_phase(1);
        let out = main();
        stop.store(true, Relaxed);
        (probe.join().expect("probe thread"), out)
    })
}

fn sum(sinks: &[&SeqSink], field: impl Fn(&SeqSink) -> &AtomicU64) -> u64 {
    sinks.iter().map(|s| field(s).load(Relaxed)).sum()
}

/// Mandatory 4. A chain of thousands of cells where a subscriber on each
/// writes the next. One trip is one trigger carried to the end.
pub fn feedback(ctx: &mut Ctx) {
    let (front, back) = build(ctx);
    let fb = &back.feedback;
    let links = ctx.sizes.feedback as u64;
    let (probe, (trips, lost)) = beside_probe(&front, ctx.ms(1000), || {
        let (mut trips, mut lost, mut seq) = (Hist::default(), 0u64, 0u64);
        let end = now() + ctx.ns(3000);
        while now() < end {
            seq += 1;
            match fb.fire(seq) {
                Some(ns) => trips.add(ns),
                None => lost += 1,
            }
        }
        (trips, lost)
    });
    probe.report(&mut ctx.m, "probe", &["idle", "load"]);
    let m = &mut ctx.m;
    m.hist("fb.trip", &trips);
    m.set("fb.hop_ns", trips.q(0.5) as f64 / links as f64);
    m.set("fb.hop_ns_p99", trips.q(0.99) as f64 / links as f64);
    m.count("viol.fb.lost", lost);
    m.count("viol.fb.links", fb.link_viol.load(Relaxed));
    m.count("viol.fb.gaps", fb.sink.gaps.load(Relaxed));
    m.count("viol.fb.reorder", fb.sink.reorder.load(Relaxed));
}

/// One writer's loop over its own islands. Paced writes are not waited for.
/// A closed-loop writer waits for each write's last sink before the next.
fn island_writer(
    w: usize,
    islands: &[Island],
    posted: &[AtomicU64],
    period: Option<u64>,
    until: u64,
) -> u64 {
    let (mut next, mut writes, mut i) = (now(), 0u64, 0usize);
    while now() < until {
        if let Some(period) = period {
            next += period;
            sleep_until(next);
        }
        let seq = posted[i].load(Relaxed) + 1;
        posted[i].store(seq, Relaxed);
        let t0 = now();
        adapter::post(KEY_WRITER + w as u64, || islands[i].src.set((seq, t0)));
        if period.is_none() {
            wait_for(5_000_000_000, || islands[i].last().seq.load(Relaxed) >= seq);
        }
        writes += 1;
        i = (i + 1) % islands.len();
    }
    writes
}

/// Mandatory 6. `4 * scale` writers, each with its own islands. Phase 1:
/// one writer alone at 1 kHz. Phase 2: all at 1 kHz. Phase 3: one writer
/// closed-loop. Phase 4: all closed-loop.
pub fn writers(ctx: &mut Ctx) {
    let (front, back) = build(ctx);
    let islands = &back.islands;
    let writers = 4 * ctx.scale;
    let per = islands.len() / writers;
    let posted: Vec<AtomicU64> = (0..islands.len()).map(|_| AtomicU64::new(0)).collect();
    let take = || -> Hist {
        adapter::flush();
        std::thread::sleep(Duration::from_millis(100));
        Hist(
            islands
                .iter()
                .flat_map(|island| island.last().take_lat())
                .collect(),
        )
    };
    let phase = |n: usize, active: usize, period: Option<u64>, ms: u64| -> (Hist, f64) {
        set_phase(n);
        let until = now() + ms * 1_000_000;
        let writes: u64 = std::thread::scope(|s| {
            let handles: Vec<_> = (0..active)
                .map(|w| {
                    let (islands, posted) = (
                        &islands[w * per..(w + 1) * per],
                        &posted[w * per..(w + 1) * per],
                    );
                    s.spawn(move || island_writer(w, islands, posted, period, until))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("writer thread"))
                .sum()
        });
        (take(), writes as f64 / (ms as f64 / 1e3))
    };
    let (probe, (solo, all, thr1, thr_all)) = beside_probe(&front, 0, || {
        let (solo, _) = phase(1, 1, Some(1_000_000), ctx.ms(1500));
        let (all, _) = phase(2, writers, Some(1_000_000), ctx.ms(3000));
        let (_, thr1) = phase(3, 1, None, ctx.ms(1000));
        let (_, thr_all) = phase(4, writers, None, ctx.ms(1500));
        (solo, all, thr1, thr_all)
    });
    wait_for(5_000_000_000, || {
        islands.iter().zip(&posted).all(|(island, posted)| {
            island
                .sinks
                .iter()
                .all(|sink| sink.seq.load(Relaxed) >= posted.load(Relaxed))
        })
    });
    let sinks: Vec<&SeqSink> = islands
        .iter()
        .flat_map(|i| i.sinks.iter().map(|s| &**s))
        .collect();
    let fires = sum(&sinks, |s| &s.fires);
    let want: u64 = posted.iter().map(|p| p.load(Relaxed)).sum::<u64>() * ISLAND_CHAINS as u64;
    probe.report(
        &mut ctx.m,
        "probe",
        &["idle", "solo", "all", "thr1", "thr_all"],
    );
    let m = &mut ctx.m;
    m.count("wr.writers", writers as u64);
    m.hist("wr.solo", &solo);
    m.hist("wr.all", &all);
    m.set(
        "wr.all_over_solo_p99",
        all.q(0.99) as f64 / solo.q(0.99).max(1) as f64,
    );
    m.set("wr.thr1_per_s", thr1);
    m.set("wr.thr_all_per_s", thr_all);
    m.set("wr.speedup", thr_all / thr1.max(1.0));
    m.count("viol.wr.missing", want - fires.min(want));
    m.count("viol.wr.gaps", sum(&sinks, |s| &s.gaps));
    m.count("viol.wr.reorder", sum(&sinks, |s| &s.reorder));
}

fn dag_report(ctx: &mut Ctx, dag: &Dag, finals: &[[u32; PAIRS]]) {
    let m = &mut ctx.m;
    m.count("viol.dag.glitch", dag.viol.glitch.load(Relaxed));
    m.count("viol.dag.torn", dag.viol.torn.load(Relaxed));
    m.count("viol.dag.reorder", dag.viol.reorder.load(Relaxed));
    m.count("viol.dag.unconverged", dag.unconverged(finals));
}

/// Writers `0..n` each own one pair in every region and post `seq`s to
/// regions in turn until `until`, flushing every 256 writes so the queue
/// stays bounded. `base[r]` is the last sequence pair 0 of region `r` already
/// took. Returns the last sequence per region and pair.
fn dag_writers(
    dag: &Dag,
    n: usize,
    base: &[u32],
    until: impl Fn(u64) -> bool + Sync,
) -> Vec<[u32; PAIRS]> {
    let regions = dag.regions.len();
    let lasts: Vec<Vec<u32>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..n)
            .map(|pair| {
                let until = &until;
                s.spawn(move || {
                    let mut last = if pair == 0 {
                        base.to_vec()
                    } else {
                        vec![1u32; regions]
                    };
                    let mut writes = 0u64;
                    while !until(writes) {
                        let r = (writes as usize) % regions;
                        last[r] += 1;
                        dag.write(KEY_WRITER + pair as u64, r, pair, last[r], false);
                        writes += 1;
                        if writes.is_multiple_of(256) {
                            adapter::flush();
                        }
                    }
                    last
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("dag writer"))
            .collect()
    });
    adapter::flush();
    std::thread::sleep(Duration::from_millis(200));
    (0..regions)
        .map(|r| std::array::from_fn(|p| lasts.get(p).map_or(1, |l| l[r])))
        .collect()
}

/// Mandatory 8. Phase 1: one writer, 2 kHz, every write waited for, so each
/// sink must fire exactly once per write that reaches it. Phase 2: four
/// writers at once into the same regions for two seconds.
pub fn diamond(ctx: &mut Ctx) {
    let (front, back) = build(ctx);
    let dag = &back.dag;
    let regions = dag.regions.len();
    let (probe, (lat, writes1, dup, coalesced, finals, per_s)) = beside_probe(&front, 0, || {
        let mut written = vec![0u64; regions];
        let (mut next, end, mut n) = (now(), now() + ctx.ns(2000), 0usize);
        while now() < end {
            next += 500_000;
            sleep_until(next);
            let r = n % regions;
            written[r] += 1;
            dag.write(KEY_WRITER, r, 0, written[r] as u32 + 1, true);
            n += 1;
        }
        adapter::flush();
        std::thread::sleep(Duration::from_millis(100));
        let (mut dup, mut coalesced, mut lat) = (0u64, 0u64, Hist::default());
        for (region, written) in dag.regions.iter().zip(&written) {
            for sink in &region.sinks {
                let want = if sink.deps & 0b11 != 0 { *written } else { 0 };
                let fires = sink.fires.load(Relaxed);
                dup += fires.saturating_sub(want);
                coalesced += want.saturating_sub(fires);
                lat.0.append(&mut sink.lat.lock());
            }
        }
        set_phase(2);
        let end = now() + ctx.ns(2000);
        let total = AtomicU64::new(0);
        let base: Vec<u32> = written.iter().map(|w| *w as u32 + 1).collect();
        let finals = dag_writers(dag, PAIRS, &base, |writes| {
            if writes.is_multiple_of(64) {
                total.fetch_add(64, Relaxed);
            }
            now() > end
        });
        (
            lat,
            n as u64,
            dup,
            coalesced,
            finals,
            total.load(Relaxed) as f64 / (ctx.ns(2000) as f64 / 1e9),
        )
    });
    probe.report(&mut ctx.m, "probe", &["idle", "one", "four"]);
    dag_report(ctx, dag, &finals);
    let m = &mut ctx.m;
    m.hist("dia", &lat);
    m.count("dia.writes", writes1);
    m.count("viol.dia.dup_fires", dup);
    m.count("dia.coalesced", coalesced);
    m.set("dia.four_writes_per_s", per_s);
}

/// Mandatory 9. Phase 1: one 200-level chain per write, 500 a second.
/// Phase 2: a group of 40 chains per write, closed-loop.
pub fn deep(ctx: &mut Ctx) {
    let (front, back) = build(ctx);
    let deep = &back.deep;
    let chains = deep.srcs.len();
    let mut seqs = vec![0u64; chains];
    let (probe, (one, mid, group)) = beside_probe(&front, 0, || {
        let (mut next, end, mut n) = (now(), now() + ctx.ns(2000), 0usize);
        while now() < end {
            next += 2_000_000;
            sleep_until(next);
            let c = n % chains;
            seqs[c] += 1;
            let (seq, t0) = (seqs[c], now());
            adapter::post(KEY_WRITER, || deep.srcs[c].set((seq, t0)));
            n += 1;
        }
        wait_for(5_000_000_000, || {
            (0..chains).all(|c| deep.end[c].seq.load(Relaxed) >= seqs[c])
        });
        let one = Hist(deep.end.iter().flat_map(|s| s.take_lat()).collect());
        let mid = Hist(deep.mid.iter().flat_map(|s| s.take_lat()).collect());
        set_phase(2);
        let (mut group, end, mut g) = (Hist::default(), now() + ctx.ns(2000), 0usize);
        while now() < end {
            let range = g * DEEP_WIDTH..(g + 1) * DEEP_WIDTH;
            let t0 = now();
            adapter::post(KEY_WRITER, || {
                for c in range.clone() {
                    seqs[c] += 1;
                    deep.srcs[c].set((seqs[c], t0));
                }
            });
            if wait_for(5_000_000_000, || {
                range
                    .clone()
                    .all(|c| deep.end[c].seq.load(Relaxed) >= seqs[c])
            }) {
                let last = range
                    .clone()
                    .map(|c| deep.end[c].ns.load(Relaxed))
                    .max()
                    .unwrap_or(t0);
                group.add(last.saturating_sub(t0));
            }
            g = (g + 1) % (chains / DEEP_WIDTH);
        }
        (one, mid, group)
    });
    let sinks: Vec<&SeqSink> = deep.end.iter().chain(&deep.mid).map(|s| &**s).collect();
    probe.report(&mut ctx.m, "probe", &["idle", "one", "group"]);
    let m = &mut ctx.m;
    m.hist("deep.one", &one);
    m.hist("deep.mid", &mid);
    m.hist("deep.group", &group);
    m.set("deep.ns_per_level", one.q(0.5) as f64 / DEEP_LEVELS as f64);
    m.set(
        "deep.200_over_100",
        one.q(0.5) as f64 / mid.q(0.5).max(1) as f64,
    );
    m.count("viol.deep.gaps", sum(&sinks, |s| &s.gaps));
    m.count("viol.deep.reorder", sum(&sinks, |s| &s.reorder));
    m.count(
        "viol.deep.missing",
        (0..chains)
            .filter(|c| deep.end[*c].seq.load(Relaxed) < seqs[*c])
            .count() as u64,
    );
}

/// S9. A keyed table of `10,000 * scale` rows. Demand goes 0, 100, all, 0,
/// three times. At each 0 the scenario rewrites 2,000 rows and records the
/// time, hyphae's fire count, and resident memory.
pub fn demand(ctx: &mut Ctx) {
    let keys = 10_000 * ctx.scale as u32;
    let rows = hyphae::CellMap::<u32, u64>::new();
    rows.insert_many((0..keys).map(|k| (k, 0)).collect());
    let table = Table::new(&rows);
    let hits = std::sync::Arc::new(AtomicU64::new(0));
    let mut rng = Rng::new(41);
    let mut version = 0u64;
    let fires = || {
        adapter::counters()
            .iter()
            .find(|(n, _)| n == "fires")
            .map_or(0, |(_, v)| *v)
    };
    let mut rewrite = |n: usize| -> (f64, f64, f64) {
        adapter::flush();
        let (t0, f0, e0) = (now(), fires(), util::evals());
        for _ in 0..n {
            version += 1;
            let k = rng.below(keys as usize) as u32;
            adapter::write(KEY_WRITER, || rows.insert(k, version));
        }
        adapter::flush();
        let n = n as f64;
        (
            (now() - t0) as f64 / 1e3 / n,
            (fires() - f0) as f64 / n,
            (util::evals() - e0) as f64 / n,
        )
    };
    let mut zeros = Vec::new();
    let mut peak = 0u64;
    zeros.push((rewrite(2000), util::rss_bytes()));
    for _ in 0..3 {
        for demand in [100, keys] {
            let readers: Vec<_> = (0..demand)
                .map(|k| {
                    let key = Cell::new(k);
                    let out = table
                        .lookup(&key)
                        .map(|v| {
                            count_eval();
                            *v
                        })
                        .materialize();
                    let seen = std::sync::Arc::clone(&hits);
                    let guard = out.subscribe(move |_| {
                        seen.fetch_add(1, Relaxed);
                    });
                    (key, out, guard)
                })
                .collect();
            rewrite(2000);
            peak = peak.max(util::rss_bytes());
            drop(readers);
        }
        adapter::flush();
        std::thread::sleep(Duration::from_millis(300));
        zeros.push((rewrite(2000), util::rss_bytes()));
    }
    let ((us0, fires0, _), rss0) = zeros[1];
    let ((us1, fires1, evals1), rss1) = zeros[3];
    let ((us_fresh, fires_fresh, _), _) = zeros[0];
    let m = &mut ctx.m;
    m.set("demand.zero_write_us.fresh", us_fresh);
    m.set("demand.zero_write_us.first", us0);
    m.set("demand.zero_write_us.last", us1);
    m.set("demand.zero_write_cost_ratio", us1 / us_fresh.max(1e-9));
    m.set("demand.zero_fires_per_write.fresh", fires_fresh);
    m.set("demand.zero_fires_per_write.first", fires0);
    m.set("demand.zero_fires_per_write.last", fires1);
    m.set("demand.zero_reader_evals_per_write", evals1);
    m.set("demand.rss_zero_first_mb", rss0 as f64 / 1e6);
    m.set("demand.rss_zero_last_mb", rss1 as f64 / 1e6);
    m.set("demand.rss_peak_mb", peak as f64 / 1e6);
    m.set("demand.rss_growth", rss1 as f64 / rss0 as f64 - 1.0);
    m.set(
        "demand.leak_of_peak",
        (rss1 as f64 - rss0 as f64) / (peak as f64 - rss0 as f64).max(1.0),
    );
    m.count("demand.reader_hits", hits.load(Relaxed));
}

/// S7, reduced. Four writers post to every region of the reconvergent
/// graphs, half of them wired at random, while four more insert, update
/// and remove rows of keyed inputs. Then one writer posts the same 20,000
/// writes to two fresh graphs, and every sink's observation sequence must
/// be the same in both (L1-8).
pub fn oracle(ctx: &mut Ctx) {
    let (front, back) = build(ctx);
    let dag = &back.dag;
    let each = ctx.writes / 8;
    let (probe, finals) = beside_probe(&front, 0, || {
        std::thread::scope(|s| {
            for w in 0..4usize {
                let check = &dag.maps[w];
                s.spawn(move || {
                    let mut rng = Rng::new(100 + w as u64);
                    for n in 0..each {
                        let k = rng.below(MAP_KEYS as usize) as u32;
                        if rng.below(4) == 0 {
                            adapter::post(KEY_WRITER + 10 + w as u64, || {
                                check.input.remove(&k);
                            });
                        } else {
                            adapter::post(KEY_WRITER + 10 + w as u64, || {
                                check.input.insert(k, n);
                            });
                        }
                        if n.is_multiple_of(256) {
                            adapter::flush();
                        }
                    }
                });
            }
            dag_writers(dag, PAIRS, &vec![1; dag.regions.len()], |writes| {
                writes >= each
            })
        })
    });
    adapter::flush();
    std::thread::sleep(Duration::from_millis(300));
    probe.report(&mut ctx.m, "probe", &["idle", "load"]);
    dag_report(ctx, dag, &finals);
    ctx.m.count("viol.map.bad_diff", dag.map_viol.load(Relaxed));
    ctx.m.count("viol.map.diverged", dag.maps_diverged());
    ctx.m.count("oracle.writes", each * 8);

    let replay_writes = ctx.writes.min(20_000);
    let replay = || -> Vec<u64> {
        let dag = Dag::build(64, 0xD17);
        let mut rng = Rng::new(77);
        let mut last = vec![[1u32; PAIRS]; 64];
        for n in 0..replay_writes {
            let (r, p) = (rng.below(64), rng.below(PAIRS));
            last[r][p] += 1;
            dag.write(KEY_WRITER + p as u64, r, p, last[r][p], false);
            if n.is_multiple_of(256) {
                adapter::flush();
            }
        }
        adapter::flush();
        std::thread::sleep(Duration::from_millis(300));
        dag.regions
            .iter()
            .flat_map(|r| r.sinks.iter().map(|s| s.hash.load(Relaxed)))
            .collect()
    };
    let (a, b) = (replay(), replay());
    ctx.m.count(
        "viol.det.sinks_differ",
        a.iter().zip(&b).filter(|(x, y)| x != y).count() as u64,
    );
}

/// Proves the checkers can fail. Writes that break each rule on purpose
/// must raise the matching count: a sequence that goes backwards, a pair
/// written as two writes, a tick that skips a number, and a tick that goes
/// back. `selftest.glitch_possible` is how many glitches raw unbatched
/// writes produced, which is above 0 only on an engine that can glitch.
pub fn selftest(ctx: &mut Ctx) {
    let dag = Dag::build(8, 3);
    dag.write(KEY_WRITER, 0, 0, 9, true);
    dag.write(KEY_WRITER, 0, 0, 5, true);
    adapter::flush();
    ctx.m
        .count("selftest.reorder", dag.viol.reorder.load(Relaxed));
    let inputs = &dag.regions[2].inputs;
    let mut one = hyphae::Gettable::get(&inputs[2]);
    (one.s[2], one.t0) = (7, now());
    adapter::write(KEY_WRITER, || inputs[2].set(one));
    adapter::flush();
    ctx.m.count("selftest.torn", dag.viol.torn.load(Relaxed));
    for seq in 2..200u32 {
        for i in 0..super::dag::INPUTS {
            let mut v = hyphae::Gettable::get(&dag.regions[4].inputs[i]);
            (v.s[i], v.t0) = (seq, now());
            dag.regions[4].inputs[i].set(v);
        }
    }
    adapter::flush();
    ctx.m
        .count("selftest.glitch_possible", dag.viol.glitch.load(Relaxed));

    // The same reconvergent graph built as event cells, written in batches.
    let events = adapter::events(|| Dag::build(8, 3));
    for seq in 2..2000u32 {
        events.write(
            KEY_WRITER,
            (seq % 8) as usize,
            (seq % 4) as usize,
            seq,
            true,
        );
    }
    adapter::flush();
    ctx.m.count(
        "selftest.event_cell_glitches",
        events.viol.glitch.load(Relaxed),
    );
    ctx.m
        .count("selftest.event_cell_torn", events.viol.torn.load(Relaxed));

    let front = Front::build(&super::universe::Sizes::new(1, 1.0, 100.0));
    for t in [1, 2, 4, 3] {
        front.tick.fire(t);
        adapter::flush();
        std::thread::sleep(Duration::from_millis(20));
    }
    ctx.m
        .count("selftest.tick_gaps", front.tick.acct.gaps.load(Relaxed));
    ctx.m.count(
        "selftest.tick_reorder",
        front.tick.acct.reorder.load(Relaxed),
    );
    let sink = SeqSink::new(false);
    for seq in [1, 2, 5, 4] {
        sink.hit(seq, 0);
    }
    ctx.m.count("selftest.seq_gaps", sink.gaps.load(Relaxed));
    ctx.m
        .count("selftest.seq_reorder", sink.reorder.load(Relaxed));
}
