//! Scenarios that ride the 30 Hz tick.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use hyphae::{Cell, CellMap, MapDiff, MapExt, Materialize, Mutable, Watchable};

use super::{
    Ctx, adapter,
    load::{HZ30, PHASE, ProbeLog, TickLog, drive_until, probe_loop, set_phase},
    universe::{
        Acct, Back, Front, KEY_EDIT, KEY_PROBE, KEY_WRITER, MAX_TICKS, Probe, TABLE_ROWS,
        TIMER_SOURCES, build_stream, row_value,
    },
    util::{self, Hist, Metrics, Rng, now, sleep_until, wait_for},
};

/// Build the whole graph with nothing else running, and record what it cost.
///
/// Only `tick` loads the store the way a server does, one row write and
/// then that row's pipeline, because only `tick` gates the setup time. The
/// others insert every row in one write first. The graph is the same.
pub fn build(ctx: &mut Ctx) -> (Front, Back) {
    let rss0 = util::rss_bytes();
    let started = Instant::now();
    let front = Front::build(&ctx.sizes);
    let back = Back::build(&ctx.sizes, ctx.row_by_row);
    adapter::flush();
    ctx.m.set("setup.ms", started.elapsed().as_secs_f64() * 1e3);
    ctx.m.set(
        "setup.rss_mb",
        (util::rss_bytes() as f64 - rss0 as f64) / 1e6,
    );
    (front, back)
}

pub struct Ride<R> {
    pub ticks: TickLog,
    pub probe: ProbeLog,
    pub side: R,
    /// Cores this process used while the trigger ran.
    pub cores: f64,
    /// hyphae's own counters over the same window.
    pub counters: Vec<(String, u64)>,
}

/// Run the trigger at 30 Hz with the disjoint probe at about 200 Hz and
/// `side` beside it. Phase 0 is `idle_ms` with no trigger; phase 1 starts
/// with the trigger. `side` runs from the start of phase 0 until `stop`.
pub fn ride<R: Send>(
    ctx: &Ctx,
    front: &Front,
    acct: &Acct,
    fire: impl Fn(u64),
    idle_ms: u64,
    side: impl FnOnce(&AtomicBool) -> R + Send,
) -> Ride<R> {
    let stop = AtomicBool::new(false);
    let end = AtomicBool::new(false);
    set_phase(0);
    std::thread::scope(|s| {
        let probe = s.spawn(|| probe_loop(&stop, 5_000_000, 7, |seq| front.probe.fire(seq)));
        let side = s.spawn(|| side(&stop));
        std::thread::sleep(Duration::from_millis(idle_ms));
        set_phase(1);
        let (cpu0, t0, ctr0) = (
            util::self_cpu_seconds(),
            Instant::now(),
            adapter::counters(),
        );
        let ticks = drive_until(acct, 1, ctx.warm + ctx.ticks, HZ30, &end, fire);
        let cores = (util::self_cpu_seconds() - cpu0) / t0.elapsed().as_secs_f64();
        let counters = adapter::counters()
            .into_iter()
            .zip(ctr0)
            .map(|((name, after), (_, before))| (name, after.saturating_sub(before)))
            .collect();
        stop.store(true, Relaxed);
        Ride {
            ticks,
            probe: probe.join().expect("probe thread"),
            side: side.join().expect("side thread"),
            cores,
            counters,
        }
    })
}

fn report<R>(m: &mut Metrics, ride: &Ride<R>, name: &str, warm: u64, acct: &Acct, phases: &[&str]) {
    ride.ticks.report(m, name, warm as usize, acct);
    ride.probe.report(m, "probe", phases);
    m.set("cpu.cores", ride.cores);
    let ticks = ride.ticks.comp.len().max(1) as f64;
    for (name, delta) in &ride.counters {
        if !name.starts_with("max") && !name.starts_with("widest") {
            m.set(&format!("ride.{name}_per_tick"), *delta as f64 / ticks);
        }
    }
}

/// S1. The tick alone, with the disjoint probe beside it.
pub fn tick(ctx: &mut Ctx) {
    let (front, _back) = build(ctx);
    let ride = ride(
        ctx,
        &front,
        &front.tick.acct,
        |t| front.tick.fire(t),
        0,
        |_| (),
    );
    report(
        &mut ctx.m,
        &ride,
        "tick",
        ctx.warm,
        &front.tick.acct,
        &["idle", "load"],
    );
}

/// S2. The disjoint probe idle, then under the tick.
pub fn probe(ctx: &mut Ctx) {
    let (front, _back) = build(ctx);
    let ride = ride(
        ctx,
        &front,
        &front.tick.acct,
        |t| front.tick.fire(t),
        ctx.ms(1500),
        |_| (),
    );
    report(
        &mut ctx.m,
        &ride,
        "tick",
        ctx.warm,
        &front.tick.acct,
        &["idle", "load"],
    );
}

/// S3. A probe that writes a parameter cell inside the tick's own scene: a
/// node the tick never writes, feeding a join the tick fires every time.
pub fn shared(ctx: &mut Ctx) {
    let (front, _back) = build(ctx);
    let tick = &front.tick;
    let ride = ride(
        ctx,
        &front,
        &tick.acct,
        |t| tick.fire(t),
        ctx.ms(1500),
        |stop| {
            let mut rng = Rng::new(11);
            probe_loop(stop, 5_000_000, 13, |seq| {
                let i = rng.below(tick.params.len());
                let t0 = now();
                adapter::post(KEY_PROBE + 1, || tick.params[i].set(seq));
                wait_for(2_000_000_000, || tick.acct.param_seen(i).0 == seq)
                    .then(|| tick.acct.param_seen(i).1.saturating_sub(t0))
            })
        },
    );
    report(
        &mut ctx.m,
        &ride,
        "tick",
        ctx.warm,
        &tick.acct,
        &["idle", "load"],
    );
    ride.side.report(&mut ctx.m, "shared", &["idle", "load"]);
}

/// S13. A 1 kHz pulse train of distinct values through a five-cell chain to
/// a counting sink, during the tick. Pulses are not waited for.
pub fn pulse(ctx: &mut Ctx) {
    let (front, _back) = build(ctx);
    let train = Probe::build(true);
    let ride = ride(
        ctx,
        &front,
        &front.tick.acct,
        |t| front.tick.fire(t),
        0,
        |stop| {
            let mut sent = 0u64;
            let mut next = now();
            while !stop.load(Relaxed) {
                next += 1_000_000;
                sleep_until(next);
                sent += 1;
                let t0 = now();
                adapter::post(KEY_PROBE + 2, || train.src.set((sent, t0)));
            }
            sent
        },
    );
    let sent = ride.side;
    wait_for(5_000_000_000, || train.sink.seq.load(Relaxed) >= sent);
    report(
        &mut ctx.m,
        &ride,
        "tick",
        ctx.warm,
        &front.tick.acct,
        &["idle", "load"],
    );
    let m = &mut ctx.m;
    m.count("pulse.sent", sent);
    m.count(
        "viol.pulse.missing",
        sent - train.sink.fires.load(Relaxed).min(sent),
    );
    m.count("viol.pulse.gaps", train.sink.gaps.load(Relaxed));
    m.count("viol.pulse.reorder", train.sink.reorder.load(Relaxed));
    m.hist("pulse", &Hist(train.sink.take_lat()));
}

fn count_diff(diff: &MapDiff<u32, u64>) -> u64 {
    match diff {
        MapDiff::Insert { .. } => 1,
        MapDiff::Batch { changes } => changes.iter().map(count_diff).sum(),
        _ => 0,
    }
}

/// S12. A control writer inserts 10,000 rows, one write each, into a
/// collection the tick never reads (phase 2), then 1,000 rows into the
/// table the tick's lookups read (phase 3), and repeats.
pub fn burst(ctx: &mut Ctx) {
    let (front, _back) = build(ctx);
    let tick = &front.tick;
    let other = CellMap::<u32, u64>::new();
    let inserts = Arc::new(AtomicU64::new(0));
    let seen = Arc::clone(&inserts);
    let _diffs = other.subscribe_diffs(move |diff| {
        seen.fetch_add(count_diff(diff), Relaxed);
    });
    let ride = ride(
        ctx,
        &front,
        &tick.acct,
        |t| tick.fire(t),
        0,
        |stop| {
            let (mut unread, mut read, mut sent) = (Hist::default(), Hist::default(), 0u64);
            std::thread::sleep(Duration::from_millis(700));
            while !stop.load(Relaxed) {
                set_phase(2);
                let t0 = now();
                for k in 0..10_000u32 {
                    adapter::write(KEY_EDIT, || other.insert(k, 1));
                }
                unread.add(now() - t0);
                sent += 10_000;
                set_phase(3);
                let t0 = now();
                for k in 0..1_000u32 {
                    adapter::write(KEY_EDIT, || tick.rows.insert(TABLE_ROWS + k, 1));
                }
                read.add(now() - t0);
                set_phase(1);
                adapter::write(KEY_EDIT, || other.remove_many((0..10_000).collect()));
                adapter::write(KEY_EDIT, || {
                    tick.rows
                        .remove_many((TABLE_ROWS..TABLE_ROWS + 1_000).collect())
                });
                std::thread::sleep(Duration::from_millis(200));
            }
            (unread, read, sent)
        },
    );
    adapter::flush();
    report(
        &mut ctx.m,
        &ride,
        "tick",
        ctx.warm,
        &tick.acct,
        &["idle", "quiet", "unread", "read"],
    );
    let (unread, read, sent) = &ride.side;
    let m = &mut ctx.m;
    m.hist("tick.unread", &ride.ticks.hist(0, Some(2)));
    m.hist("tick.read", &ride.ticks.hist(0, Some(3)));
    m.hist("burst.unread_total", unread);
    m.hist("burst.read_total", read);
    m.count(
        "viol.burst.missing",
        sent - inserts.load(Relaxed).min(*sent),
    );
}

/// A sink for a stream added while the tick runs: every tick after the
/// first it sees must follow the one before.
#[derive(Default)]
struct LateSink {
    last: AtomicU64,
    gaps: AtomicU64,
    reorder: AtomicU64,
}

/// S11. Streams are added to and removed from the tick's scene while it
/// runs, 20 edits a second.
pub fn edit(ctx: &mut Ctx) {
    let (front, _back) = build(ctx);
    let tick = &front.tick;
    let late = Arc::new(LateSink::default());
    let ride = ride(
        ctx,
        &front,
        &tick.acct,
        |t| tick.fire(t),
        0,
        |stop| {
            let (mut add, mut remove) = (Hist::default(), Hist::default());
            let mut live = VecDeque::new();
            let mut rng = Rng::new(17);
            while !stop.load(Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
                if live.len() < 50 {
                    let (param, key) = (
                        Cell::new(0u64),
                        Cell::new(rng.below(TABLE_ROWS as usize) as u32),
                    );
                    let (seen, last) = (Arc::clone(&late), AtomicU64::new(u64::MAX));
                    let t0 = now();
                    let keep = build_stream(
                        &tick.sources[rng.below(TIMER_SOURCES)],
                        &param,
                        &key,
                        &tick.table,
                        move |t, _, _| {
                            let prev = last.swap(t, Relaxed);
                            seen.last.store(t, Relaxed);
                            if prev != u64::MAX && t > prev + 1 {
                                seen.gaps.fetch_add(t - prev - 1, Relaxed);
                            } else if prev != u64::MAX && t < prev {
                                seen.reorder.fetch_add(1, Relaxed);
                            }
                        },
                    );
                    add.add(now() - t0);
                    live.push_back((keep, param, key));
                } else {
                    let t0 = now();
                    drop(live.pop_front());
                    remove.add(now() - t0);
                }
            }
            (add, remove)
        },
    );
    report(
        &mut ctx.m,
        &ride,
        "tick",
        ctx.warm,
        &tick.acct,
        &["idle", "load"],
    );
    let m = &mut ctx.m;
    m.hist("edit.add", &ride.side.0);
    m.hist("edit.remove", &ride.side.1);
    m.count("viol.edit.gaps", late.gaps.load(Relaxed));
    m.count("viol.edit.reorder", late.reorder.load(Relaxed));
    m.count(
        "viol.edit.never_fired",
        u64::from(late.last.load(Relaxed) == 0),
    );
}

/// S4 and mandatory 2. About 1,000 operations a second beside the tick.
/// Half change a value. A quarter change a lookup key. A quarter add a
/// request (a keyed pipeline over the store with a subscriber) and remove
/// the oldest.
pub fn churn(ctx: &mut Ctx) {
    let (front, back) = build(ctx);
    let (tick, store) = (&front.tick, &back.store);
    let pool = 1_000 * ctx.scale;
    let request = |k: u32| {
        let cell = store.map.get(&k).map(|v| v.unwrap_or(0)).materialize();
        let guard = cell.subscribe(|_| {});
        (cell, guard)
    };
    let mut rng = Rng::new(23);
    let mut live: VecDeque<_> = (0..pool)
        .map(|_| request(rng.below(ctx.sizes.store_keys) as u32))
        .collect();
    let keys_now: Vec<AtomicU64> = (0..tick.keys.len())
        .map(|i| AtomicU64::new(i as u64 % u64::from(TABLE_ROWS)))
        .collect();
    let ride = ride(
        ctx,
        &front,
        &tick.acct,
        |t| tick.fire(t),
        0,
        |stop| {
            let (mut value, mut key, mut req, mut lost) =
                (Hist::default(), Hist::default(), Hist::default(), 0u64);
            let (mut next, mut op) = (now(), 0u64);
            while !stop.load(Relaxed) {
                next += 1_000_000;
                sleep_until(next);
                op += 1;
                let i = rng.below(tick.params.len());
                let t0 = now();
                match op % 4 {
                    0 | 2 => {
                        adapter::post(KEY_EDIT, || tick.params[i].set(op));
                        match wait_for(2_000_000_000, || tick.acct.param_seen(i).0 == op) {
                            true => value.add(tick.acct.param_seen(i).1.saturating_sub(t0)),
                            false => lost += 1,
                        }
                    }
                    1 => {
                        let k = (keys_now[i].load(Relaxed) + 1 + rng.below(200) as u64)
                            % u64::from(TABLE_ROWS);
                        keys_now[i].store(k, Relaxed);
                        adapter::post(KEY_EDIT, || tick.keys[i].set(k as u32));
                        match wait_for(2_000_000_000, || {
                            tick.acct.row_seen(i).0 == row_value(k as u32)
                        }) {
                            true => key.add(tick.acct.row_seen(i).1.saturating_sub(t0)),
                            false => lost += 1,
                        }
                    }
                    _ => {
                        live.push_back(request(rng.below(ctx.sizes.store_keys) as u32));
                        drop(live.pop_front());
                        req.add(now() - t0);
                    }
                }
                next = next.max(now());
            }
            (value, key, req, lost)
        },
    );
    adapter::flush();
    report(
        &mut ctx.m,
        &ride,
        "tick",
        ctx.warm,
        &tick.acct,
        &["idle", "load"],
    );
    let (value, key, req, lost) = &ride.side;
    let unconverged = (0..tick.keys.len())
        .filter(|i| tick.acct.row_seen(*i).0 != row_value(keys_now[*i].load(Relaxed) as u32))
        .count();
    let m = &mut ctx.m;
    m.hist("churn.value", value);
    m.hist("churn.key", key);
    m.hist("churn.request", req);
    m.set(
        "churn.key_over_value",
        key.q(0.5) as f64 / value.q(0.5).max(1) as f64,
    );
    m.count("viol.churn.lost", *lost);
    m.count("viol.churn.unconverged", unconverged as u64);
}

/// Mandatory 5. The tick and the probe run on a graph that holds nothing
/// else, and the rest is built beside them: every store row is a write into
/// the hot source while its cone is being built. Phase 1 is the build,
/// phase 2 the second after it.
pub fn setup(ctx: &mut Ctx) {
    let front = Front::build(&ctx.sizes);
    let tick = &front.tick;
    let (stop, end) = (AtomicBool::new(false), AtomicBool::new(false));
    set_phase(1);
    let (ticks, probe, built_ms, _back) = std::thread::scope(|s| {
        let probe = s.spawn(|| probe_loop(&stop, 5_000_000, 7, |seq| front.probe.fire(seq)));
        let ticks = s.spawn(|| {
            drive_until(&tick.acct, 1, MAX_TICKS as u64 - 2, HZ30, &end, |t| {
                tick.fire(t)
            })
        });
        std::thread::sleep(Duration::from_millis(ctx.ms(500)));
        let started = Instant::now();
        let back = Back::build(&ctx.sizes, true);
        let built_ms = started.elapsed().as_secs_f64() * 1e3;
        set_phase(2);
        std::thread::sleep(Duration::from_millis(ctx.ms(1000)));
        end.store(true, Relaxed);
        let ticks = ticks.join().expect("tick thread");
        stop.store(true, Relaxed);
        (ticks, probe.join().expect("probe thread"), built_ms, back)
    });
    ticks.report(&mut ctx.m, "tick", ctx.warm as usize, &tick.acct);
    probe.report(&mut ctx.m, "probe", &["idle", "build", "after"]);
    ctx.m
        .hist("tick.build", &ticks.hist(ctx.warm as usize, Some(1)));
    ctx.m
        .hist("tick.after", &ticks.hist(ctx.warm as usize, Some(2)));
    ctx.m.set("setup.loaded_ms", built_ms);
}

/// Mandatory 7. Eight writers hammer their own islands for two seconds
/// (phase 2), each keeping at most two writes outstanding per island, the
/// way a bounded inbound channel does. Phase 1 is before, phase 3 after.
pub fn overload(ctx: &mut Ctx) {
    let (front, back) = build(ctx);
    let (tick, islands) = (&front.tick, &back.islands);
    let hammers = 8;
    let per = islands.len() / hammers;
    let posted: Vec<AtomicU64> = (0..islands.len()).map(|_| AtomicU64::new(0)).collect();
    let span = (ctx.warm + ctx.ticks) * HZ30 / 3_000_000;
    let ride = ride(
        ctx,
        &front,
        &tick.acct,
        |t| tick.fire(t),
        0,
        |stop| {
            std::thread::scope(|s| {
                for w in 0..hammers {
                    let (posted, stop) = (&posted, &stop);
                    s.spawn(move || {
                        while !stop.load(Relaxed) {
                            if PHASE.load(Relaxed) != 2 {
                                std::thread::sleep(Duration::from_micros(200));
                                continue;
                            }
                            for i in w * per..(w + 1) * per {
                                let seq = posted[i].load(Relaxed);
                                if seq < islands[i].last().seq.load(Relaxed) + 2 {
                                    posted[i].store(seq + 1, Relaxed);
                                    let t0 = now();
                                    adapter::post(KEY_WRITER + w as u64, || {
                                        islands[i].src.set((seq + 1, t0))
                                    });
                                }
                            }
                        }
                    });
                }
                std::thread::sleep(Duration::from_millis(span));
                set_phase(2);
                let t0 = now();
                std::thread::sleep(Duration::from_millis(span));
                set_phase(3);
                let over_ns = now() - t0;
                let drained = wait_for(10_000_000_000, || {
                    islands.iter().zip(&posted).all(|(island, posted)| {
                        let want = posted.load(Relaxed);
                        island
                            .sinks
                            .iter()
                            .all(|sink| sink.seq.load(Relaxed) >= want)
                    })
                });
                (over_ns, now() - t0 - over_ns, drained)
            })
        },
    );
    let (over_ns, drain_ns, drained) = ride.side;
    let writes: u64 = posted.iter().map(|p| p.load(Relaxed)).sum();
    let sinks = || islands.iter().flat_map(|island| island.sinks.iter());
    report(
        &mut ctx.m,
        &ride,
        "tick",
        ctx.warm,
        &tick.acct,
        &["idle", "before", "over", "after"],
    );
    let m = &mut ctx.m;
    for (phase, label) in [(1, "before"), (2, "over"), (3, "after")] {
        m.hist(
            &format!("tick.{label}"),
            &ride.ticks.hist(ctx.warm as usize, Some(phase)),
        );
    }
    m.set("over.writes_per_s", writes as f64 / (over_ns as f64 / 1e9));
    m.set("over.drain_ms", drain_ns as f64 / 1e6);
    m.hist(
        "over.write",
        &Hist(
            islands
                .iter()
                .flat_map(|island| island.last().take_lat())
                .collect(),
        ),
    );
    m.count("viol.over.undrained", u64::from(!drained));
    m.count(
        "viol.over.gaps",
        sinks().map(|s| s.gaps.load(Relaxed)).sum(),
    );
    m.count(
        "viol.over.reorder",
        sinks().map(|s| s.reorder.load(Relaxed)).sum(),
    );
}

/// Mandatory 10. One trigger, thousands of independent streams, at 30 Hz.
pub fn fanout(ctx: &mut Ctx) {
    let (front, back) = build(ctx);
    let ride = ride(ctx, &front, &back.fan.acct, |t| back.fan.fire(t), 0, |_| ());
    report(
        &mut ctx.m,
        &ride,
        "fan",
        ctx.warm,
        &back.fan.acct,
        &["idle", "load"],
    );
}

/// Mandatory 3. One row of the store is rewritten about 500 times a second,
/// idle and then under the tick. The store's cone is more than half the
/// graph; one write concerns three cells and one subscriber.
pub fn hotcone(ctx: &mut Ctx) {
    let (front, back) = build(ctx);
    let (tick, store) = (&front.tick, &back.store);
    let evals0 = util::evals();
    let ride = ride(
        ctx,
        &front,
        &tick.acct,
        |t| tick.fire(t),
        ctx.ms(1500),
        |stop| {
            let mut rng = Rng::new(29);
            let mut seqs = vec![0u64; store.sinks.len()];
            probe_loop(stop, 2_000_000, 31, |_| {
                let slot = rng.below(seqs.len());
                seqs[slot] += 1;
                store.fire(slot, seqs[slot])
            })
        },
    );
    adapter::flush();
    std::thread::sleep(Duration::from_millis(100));
    report(
        &mut ctx.m,
        &ride,
        "tick",
        ctx.warm,
        &tick.acct,
        &["idle", "load"],
    );
    ride.side.report(&mut ctx.m, "hot", &["idle", "load"]);
    let writes = ride.side.lat.len().max(1) as f64;
    ctx.m.set(
        "hot.evals_per_write",
        (util::evals() - evals0) as f64 / writes,
    );
    let sinks = &store.sinks;
    ctx.m.count(
        "viol.hot.gaps",
        sinks.iter().map(|s| s.gaps.load(Relaxed)).sum(),
    );
    ctx.m.count(
        "viol.hot.reorder",
        sinks.iter().map(|s| s.reorder.load(Relaxed)).sum(),
    );
}
