//! The cost model: what each hyphae primitive costs to build, to hold and to
//! fire, how a write's cost grows with what it reaches, where equal values
//! still propagate, and what is coalesced when writers contend.
//!
//! One thread writes, inside a batch, the way myko does. Every number is the
//! best of three repeats, so that it is the cost of the primitive and not of
//! whatever else the machine was doing.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering::Relaxed},
};

use hyphae::{
    Cell, CellImmutable, CellMap, CellMutable, DedupedExt, Gettable, JoinExt, MapExt, MapQuery,
    MapValuesExt, Materialize, Mutable, Signal, SubscriptionGuard, Watchable,
};

use super::{
    Ctx, adapter,
    adapter::Table,
    universe::Keep,
    util::{self, Metrics, now},
};

type Src = Cell<u64, CellMutable>;
type Node = Cell<u64, CellImmutable>;

fn write(f: impl FnOnce()) {
    adapter::write(9, f);
}

/// Nanoseconds per call of `f`, best of three repeats of `n` calls.
fn per_op(n: u64, mut f: impl FnMut(u64)) -> f64 {
    let mut seq = 0u64;
    (0..3)
        .map(|_| {
            let t0 = now();
            for _ in 0..n {
                seq += 1;
                f(seq);
            }
            (now() - t0) as f64 / n as f64
        })
        .fold(f64::MAX, f64::min)
}

/// Build `n` of something. Records nanoseconds and resident bytes per item,
/// and nanoseconds per item to drop them if `drop_too`.
fn build<T>(m: &mut Metrics, name: &str, n: usize, mut f: impl FnMut(usize) -> T) -> Vec<T> {
    let (rss0, t0) = (util::rss_bytes() as f64, now());
    let items: Vec<T> = (0..n).map(&mut f).collect();
    m.set(&format!("build_ns.{name}"), (now() - t0) as f64 / n as f64);
    m.set(
        &format!("bytes.{name}"),
        (util::rss_bytes() as f64 - rss0) / n as f64,
    );
    items
}

fn drop_timed<T>(m: &mut Metrics, name: &str, items: Vec<T>) {
    let (n, t0) = (items.len().max(1), now());
    drop(items);
    m.set(&format!("drop_ns.{name}"), (now() - t0) as f64 / n as f64);
}

fn counter() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(0))
}

fn count(cell: &Node, hits: &Arc<AtomicU64>) -> SubscriptionGuard {
    let hits = Arc::clone(hits);
    cell.subscribe(move |signal| {
        if matches!(signal, Signal::Value(_)) {
            hits.fetch_add(1, Relaxed);
        }
    })
}

fn chain(src: &Src, levels: usize, keep: &mut Keep) -> Node {
    let mut cur = src.clone().map(|v| *v).materialize();
    for _ in 1..levels {
        let next = cur.clone().map(|v| *v).materialize();
        keep.push(Box::new(cur));
        cur = next;
    }
    cur
}

fn build_costs(m: &mut Metrics) {
    let n = 100_000;
    let cells = build(m, "cell", n, |_| Cell::new(0u64));
    let maps = build(m, "map_materialize", n, |i| {
        cells[i].clone().map(|v| v + 1).materialize()
    });
    let joins = build(m, "join_map_materialize", n / 2, |i| {
        cells[2 * i]
            .clone()
            .join(cells[2 * i + 1].clone())
            .map(|v| v.0 + v.1)
            .materialize()
    });
    let subs = build(m, "subscribe", n, |i| maps[i].subscribe(|_| {}));
    drop_timed(m, "subscribe", subs);
    drop_timed(m, "join_map_materialize", joins);
    drop_timed(m, "map_materialize", maps);
    drop_timed(m, "cell", cells);

    // Ten maps fused into one cell, against ten cells.
    let src = Cell::new(0u64);
    let fused = build(m, "ten_maps_one_cell", 20_000, |_| {
        src.clone()
            .map(|v| v + 1)
            .map(|v| v + 1)
            .map(|v| v + 1)
            .map(|v| v + 1)
            .map(|v| v + 1)
            .map(|v| v + 1)
            .map(|v| v + 1)
            .map(|v| v + 1)
            .map(|v| v + 1)
            .map(|v| v + 1)
            .materialize()
    });
    drop(fused);

    // Readers of one key each, on one map: the cost of the next reader as
    // the map gains readers.
    for readers in [1_000usize, 10_000, 100_000] {
        let map = CellMap::<u32, u64>::new();
        map.insert_many((0..readers as u32).map(|k| (k, 0)).collect());
        let points = build(m, &format!("key_reader.of_{readers}"), readers, |k| {
            map.get(&(k as u32)).materialize()
        });
        drop_timed(m, &format!("key_reader.of_{readers}"), points);
    }

    let rows = CellMap::<u32, u64>::new();
    rows.insert_many((0..256).map(|k| (k, u64::from(k))).collect());
    let table = Table::new(&rows);
    let keys: Vec<Cell<u32, CellMutable>> = (0..20_000).map(|i| Cell::new(i % 256)).collect();
    let lookups = build(m, "key_lookup", keys.len(), |i| table.lookup(&keys[i]));
    drop_timed(m, "key_lookup", lookups);

    for rows in [100usize, 10_000] {
        let map = CellMap::<u32, u64>::new();
        map.insert_many((0..rows as u32).map(|k| (k, 0)).collect());
        let subs = build(m, &format!("subscribe_diffs.rows_{rows}"), 2_000, |_| {
            map.subscribe_diffs(|_| {})
        });
        drop_timed(m, &format!("subscribe_diffs.rows_{rows}"), subs);
    }

    let map = CellMap::<u32, u64>::new();
    let t0 = now();
    for k in 0..n as u32 {
        map.insert(k, 0);
    }
    m.set("build_ns.row_insert_each", (now() - t0) as f64 / n as f64);
    let map = CellMap::<u32, u64>::new();
    let t0 = now();
    map.insert_many((0..n as u32).map(|k| (k, 0)).collect());
    m.set("build_ns.row_insert_many", (now() - t0) as f64 / n as f64);
}

fn fire_costs(m: &mut Metrics) {
    let src = Cell::new(0u64);
    m.set("write_ns.empty_batch", per_op(200_000, |_| write(|| ())));
    m.set(
        "write_ns.set_no_dependents.batched",
        per_op(200_000, |i| write(|| src.set(i))),
    );
    m.set(
        "write_ns.set_no_dependents.unbatched",
        per_op(200_000, |i| src.set(i)),
    );

    // Depth: one write through a chain of `levels` cells to one subscriber.
    for levels in [1usize, 10, 100, 1000] {
        let (src, hits, mut keep) = (Cell::new(0u64), counter(), Keep::new());
        let end = chain(&src, levels, &mut keep);
        let _sink = count(&end, &hits);
        let writes = (200_000 / levels as u64).max(200);
        let batched = per_op(writes, |i| write(|| src.set(i)));
        let unbatched = per_op(writes, |i| src.set(i));
        m.set(
            &format!("chain.{levels}.batched_ns_per_cell"),
            batched / levels as f64,
        );
        m.set(
            &format!("chain.{levels}.unbatched_ns_per_cell"),
            unbatched / levels as f64,
        );
        m.set(
            &format!("chain.{levels}.sink_hits_per_write"),
            (hits.load(Relaxed) - 1) as f64 / (writes * 6) as f64,
        );
    }

    // Ten maps fused into one cell.
    let (src, hits) = (Cell::new(0u64), counter());
    let fused = src
        .clone()
        .map(|v| v + 1)
        .map(|v| v + 1)
        .map(|v| v + 1)
        .map(|v| v + 1)
        .map(|v| v + 1)
        .map(|v| v + 1)
        .map(|v| v + 1)
        .map(|v| v + 1)
        .map(|v| v + 1)
        .map(|v| v + 1)
        .materialize();
    let _sink = count(&fused, &hits);
    m.set(
        "chain.ten_maps_one_cell.batched_ns",
        per_op(100_000, |i| write(|| src.set(i))),
    );

    // Fan-out: one source, `fan` derived cells, and then `fan` subscribers.
    for fan in [1usize, 10, 100, 1_000, 10_000, 100_000] {
        let src = Cell::new(0u64);
        let children: Vec<Node> = (0..fan)
            .map(|_| src.clone().map(|v| v + 1).materialize())
            .collect();
        let writes = (400_000 / fan as u64).max(30);
        m.set(
            &format!("fan_cells.{fan}.batched_ns_per_cell"),
            per_op(writes, |i| write(|| src.set(i))) / fan as f64,
        );
        m.set(
            &format!("fan_cells.{fan}.unbatched_ns_per_cell"),
            per_op(writes, |i| src.set(i)) / fan as f64,
        );
        drop(children);
        let (src, hits) = (Cell::new(0u64), counter());
        let subs: Vec<SubscriptionGuard> = (0..fan)
            .map(|_| {
                let hits = Arc::clone(&hits);
                src.subscribe(move |_| {
                    hits.fetch_add(1, Relaxed);
                })
            })
            .collect();
        m.set(
            &format!("fan_subscribers.{fan}.batched_ns_per_subscriber"),
            per_op(writes, |i| write(|| src.set(i))) / fan as f64,
        );
        drop(subs);
    }

    // A join, and a diamond: how often the join fires per write.
    let (a, b, hits) = (Cell::new(0u64), Cell::new(0u64), counter());
    let joined = a.clone().join(b.clone()).map(|v| v.0 + v.1).materialize();
    let _sink = count(&joined, &hits);
    m.set("join.batched_ns", per_op(200_000, |i| write(|| a.set(i))));
    for (label, batched) in [("batched", true), ("unbatched", false)] {
        let (src, hits) = (Cell::new(0u64), counter());
        let left = src.clone().map(|v| v + 1).materialize();
        let right = src.clone().map(|v| v + 2).materialize();
        let joined = left
            .clone()
            .join(right.clone())
            .map(|v| v.0 + v.1)
            .materialize();
        let _sink = count(&joined, &hits);
        let before = hits.load(Relaxed);
        let ns = per_op(50_000, |i| {
            if batched {
                write(|| src.set(i))
            } else {
                src.set(i)
            }
        });
        m.set(&format!("diamond.{label}_ns"), ns);
        m.set(
            &format!("diamond.{label}_sink_hits_per_write"),
            (hits.load(Relaxed) - before) as f64 / 150_000.0,
        );
    }

    // A keyed lookup: a write to the row it reads, and a write to its key.
    let rows = CellMap::<u32, u64>::new();
    rows.insert_many((0..256).map(|k| (k, u64::from(k))).collect());
    let table = Table::new(&rows);
    let (key, hits) = (Cell::new(7u32), counter());
    let looked = table.lookup(&key).map(|v| v.unwrap_or(0)).materialize();
    let _sink = count(&looked, &hits);
    m.set(
        "lookup.row_write_ns",
        per_op(100_000, |i| {
            write(|| {
                rows.insert(7, i);
            })
        }),
    );
    m.set(
        "lookup.key_change_ns",
        per_op(100_000, |i| write(|| key.set((i % 200) as u32))),
    );
    let before = hits.load(Relaxed);
    m.set(
        "lookup.other_row_write_ns",
        per_op(100_000, |i| {
            write(|| {
                rows.insert(250, i);
            })
        }),
    );
    m.set(
        "lookup.other_row_write_sink_hits",
        (hits.load(Relaxed) - before) as f64 / 300_000.0,
    );
}

fn map_costs(m: &mut Metrics) {
    // A row write, against how many rows the map holds.
    for rows in [1_000usize, 100_000] {
        let map = CellMap::<u32, u64>::new();
        map.insert_many((0..rows as u32).map(|k| (k, 0)).collect());
        m.set(
            &format!("row_write.rows_{rows}.no_readers_ns"),
            per_op(100_000, |i| {
                write(|| {
                    map.insert(5, i);
                })
            }),
        );
    }
    // Against how many readers of other keys it has.
    for readers in [0usize, 1_000, 10_000, 100_000] {
        let map = CellMap::<u32, u64>::new();
        map.insert_many((0..100_001u32).map(|k| (k, 0)).collect());
        let evals = counter();
        let points: Vec<Node> = (0..readers as u32)
            .map(|k| {
                let evals = Arc::clone(&evals);
                map.get(&k)
                    .map(move |v| {
                        evals.fetch_add(1, Relaxed);
                        v.unwrap_or(0)
                    })
                    .materialize()
            })
            .collect();
        let before = evals.load(Relaxed);
        let ns = per_op(20_000, |i| {
            write(|| {
                map.insert(100_000, i);
            })
        });
        m.set(&format!("row_write.other_key_readers_{readers}.ns"), ns);
        m.set(
            &format!("row_write.other_key_readers_{readers}.reader_evals_per_write"),
            (evals.load(Relaxed) - before) as f64 / 60_000.0,
        );
        drop(points);
    }
    // Against how many subscribe_diffs subscribers it has.
    for subs in [0usize, 10, 100, 1_000, 10_000] {
        let map = CellMap::<u32, u64>::new();
        map.insert_many((0..1_000u32).map(|k| (k, 0)).collect());
        let guards: Vec<SubscriptionGuard> =
            (0..subs).map(|_| map.subscribe_diffs(|_| {})).collect();
        let writes = (2_000_000 / subs.max(1) as u64).clamp(200, 100_000);
        m.set(
            &format!("row_write.diff_subscribers_{subs}.ns"),
            per_op(writes, |i| {
                write(|| {
                    map.insert(5, i);
                })
            }),
        );
        drop(guards);
    }
    // Derived collections and whole-map readers.
    for rows in [1_000usize, 10_000] {
        let map = CellMap::<u32, u64>::new();
        map.insert_many((0..rows as u32).map(|k| (k, 0)).collect());
        let view = map.clone().map_values(|_, v| v + 1).materialize();
        m.set(
            &format!("row_write.rows_{rows}.map_values_view_ns"),
            per_op(50_000, |i| {
                write(|| {
                    map.insert(5, i);
                })
            }),
        );
        drop(view);
        let len = map.len().materialize();
        m.set(
            &format!("row_write.rows_{rows}.len_reader_ns"),
            per_op(50_000, |i| {
                write(|| {
                    map.insert(5, i);
                })
            }),
        );
        drop(len);
        let entries = map.entries().materialize();
        m.set(
            &format!("row_write.rows_{rows}.entries_reader_ns"),
            per_op(2_000, |i| {
                write(|| {
                    map.insert(5, i);
                })
            }),
        );
        drop(entries);
    }
}

/// One source, `n` consumers that each test it against their own id, and
/// three cells below each test. A write changes the answer of two consumers.
fn predicate_fan(m: &mut Metrics, n: u64, dedup: bool) {
    let (src, evals, mut keep) = (Cell::new(0u64), counter(), Keep::new());
    for id in 0..n {
        let test = src.clone().map(move |v| *v % n == id);
        let test = if dedup {
            test.deduped().materialize()
        } else {
            test.materialize()
        };
        let mut cur = test.clone().map(|hit| u64::from(*hit)).materialize();
        for _ in 0..2 {
            let evals = Arc::clone(&evals);
            let next = cur
                .clone()
                .map(move |v| {
                    evals.fetch_add(1, Relaxed);
                    *v
                })
                .materialize();
            keep.push(Box::new(cur));
            cur = next;
        }
        keep.push(Box::new((test, cur)));
    }
    let writes = (2_000_000 / n).clamp(30, 20_000);
    let before = evals.load(Relaxed);
    let ns = per_op(writes, |i| write(|| src.set(i)));
    let label = if dedup { "deduped" } else { "plain" };
    m.set(&format!("predicate_fan.{n}.{label}.write_ns"), ns);
    m.set(
        &format!("predicate_fan.{n}.{label}.cells_fired_below_per_write"),
        (evals.load(Relaxed) - before) as f64 / (writes * 3) as f64,
    );
}

/// Which primitives pass an equal value on. Each count is subscriber
/// callbacks per write of a value equal to the one already there.
fn equal_values(m: &mut Metrics) {
    let rounds = 1_000u64;
    let mut probe = |name: &str, cell: &Node, mut same: Box<dyn FnMut()>| {
        let hits = counter();
        let _sink = count(cell, &hits);
        same();
        let before = hits.load(Relaxed);
        for _ in 0..rounds {
            write(&mut same);
        }
        m.set(
            &format!("equal.{name}.callbacks_per_write"),
            (hits.load(Relaxed) - before) as f64 / rounds as f64,
        );
    };
    let src = Cell::new(1u64);
    let direct = src.clone().map(|v| *v).materialize();
    let s = src.clone();
    probe("cell_set_same_value", &direct, Box::new(move || s.set(5)));

    let src = Cell::new(1u64);
    let halved = src.clone().map(|v| v / 1_000).materialize();
    let below = halved.clone().map(|v| *v).materialize();
    let (s, mut i) = (src.clone(), 0u64);
    probe(
        "map_output_unchanged",
        &below,
        Box::new(move || {
            i += 1;
            s.set(i % 900);
        }),
    );

    let src = Cell::new(1u64);
    let quiet = src.clone().map(|v| v / 1_000).deduped().materialize();
    let below = quiet.clone().map(|v| *v).materialize();
    let (s, mut i) = (src.clone(), 0u64);
    probe(
        "map_output_unchanged_deduped",
        &below,
        Box::new(move || {
            i += 1;
            s.set(i % 900);
        }),
    );

    let (a, b) = (Cell::new(1u64), Cell::new(1u64));
    let joined = a
        .clone()
        .join(b.clone())
        .map(|v| v.0 / 1_000 + v.1)
        .materialize();
    let (s, mut i) = (a.clone(), 0u64);
    probe(
        "join_output_unchanged",
        &joined,
        Box::new(move || {
            i += 1;
            s.set(i % 900);
        }),
    );

    let map = CellMap::<u32, u64>::new();
    map.insert(1, 5);
    let point = map.get(&1).map(|v| v.unwrap_or(0)).materialize();
    let mm = map.clone();
    probe(
        "row_insert_same_value.key_reader",
        &point,
        Box::new(move || {
            mm.insert(1, 5);
        }),
    );

    let map = CellMap::<u32, u64>::new();
    map.insert(1, 5);
    let diffs = counter();
    let seen = Arc::clone(&diffs);
    let _guard = map.subscribe_diffs(move |_| {
        seen.fetch_add(1, Relaxed);
    });
    let before = diffs.load(Relaxed);
    for _ in 0..rounds {
        write(|| {
            map.insert(1, 5);
        });
    }
    let diff_rate = (diffs.load(Relaxed) - before) as f64 / rounds as f64;

    let rows = CellMap::<u32, u64>::new();
    rows.insert_many((0..8).map(|k| (k, 5)).collect());
    let table = Table::new(&rows);
    let key = Cell::new(1u32);
    let looked = table.lookup(&key).map(|v| v.unwrap_or(0)).materialize();
    let (k, mut i) = (key.clone(), 0u32);
    probe(
        "lookup_key_change_same_row_value",
        &looked,
        Box::new(move || {
            i += 1;
            k.set(i % 8);
        }),
    );
    m.set("equal.row_insert_same_value.diffs_per_write", diff_rate);

    // A derived collection whose output row does not change.
    let map = CellMap::<u32, u64>::new();
    map.insert(1, 5);
    let view = map.clone().map_values(|_, v| v / 1_000).materialize();
    let diffs = counter();
    let seen = Arc::clone(&diffs);
    let _guard = view.subscribe_diffs(move |_| {
        seen.fetch_add(1, Relaxed);
    });
    let before = diffs.load(Relaxed);
    for i in 0..rounds {
        write(|| {
            map.insert(1, i % 900);
        });
    }
    m.set(
        "equal.map_values_output_unchanged.diffs_per_write",
        (diffs.load(Relaxed) - before) as f64 / rounds as f64,
    );
}

/// The tick's shape with a stage whose output changes every eighth tick, as
/// a quantising or switching node's does: 3,600 streams of 7 cells.
fn quantised_tick(m: &mut Metrics, dedup: bool) {
    let sources: Vec<Src> = (0..34).map(|_| Cell::new(0u64)).collect();
    let (hits, mut keep) = (counter(), Keep::new());
    for i in 0..3_600usize {
        let n1 = sources[i % 34].clone().map(|v| v + 1).materialize();
        let quant = n1.clone().map(|v| v / 8);
        let quant = if dedup {
            quant.deduped().materialize()
        } else {
            quant.materialize()
        };
        let end = {
            let mut cur = quant.clone().map(|v| *v).materialize();
            for _ in 0..3 {
                let next = cur.clone().map(|v| *v).materialize();
                keep.push(Box::new(cur));
                cur = next;
            }
            cur
        };
        keep.push(Box::new(count(&end, &hits)));
        keep.push(Box::new((n1, quant, end)));
    }
    let before = hits.load(Relaxed);
    let ns = per_op(400, |t| {
        write(|| {
            for source in &sources {
                source.set(t);
            }
        })
    });
    let label = if dedup { "deduped" } else { "plain" };
    m.set(&format!("quantised_tick.{label}.tick_us"), ns / 1e3);
    m.set(
        &format!("quantised_tick.{label}.sink_hits_per_tick"),
        (hits.load(Relaxed) - before) as f64 / 1_200.0,
    );
}

/// What reaches a sink when writers contend. Each of `threads` threads
/// writes 1, 2, 3, … to its own cell, one batch per write. The cell feeds one
/// derived cell and a sink that counts the values it never saw.
fn contention(m: &mut Metrics, threads: usize, events: bool) {
    let writes = 20_000u64;
    let build = || {
        (0..threads)
            .map(|_| {
                let src = Cell::new(0u64);
                let out = src.clone().map(|v| *v).materialize();
                let (seen, missed, stale) = (counter(), counter(), counter());
                let (s, g) = (Arc::clone(&seen), Arc::clone(&missed));
                let guard = out.subscribe(move |signal| {
                    if let Signal::Value(v) = signal {
                        let prev = s.swap(**v, Relaxed);
                        if **v > prev + 1 {
                            g.fetch_add(**v - prev - 1, Relaxed);
                        }
                    }
                });
                (src, out, seen, missed, stale, guard)
            })
            .collect::<Vec<_>>()
    };
    let lanes = if events {
        adapter::events(build)
    } else {
        build()
    };
    let t0 = now();
    std::thread::scope(|s| {
        for (src, out, _, _, stale, _) in &lanes {
            s.spawn(move || {
                for i in 1..=writes {
                    write(|| src.set(i));
                    // The writer reads its own write back through the derived cell.
                    if out.get() != i {
                        stale.fetch_add(1, Relaxed);
                    }
                }
            });
        }
    });
    let elapsed = now() - t0;
    adapter::flush();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let total = (writes * threads as u64) as f64;
    let sum = |f: fn(
        &(
            Src,
            Node,
            Arc<AtomicU64>,
            Arc<AtomicU64>,
            Arc<AtomicU64>,
            SubscriptionGuard,
        ),
    ) -> u64|
     -> f64 { lanes.iter().map(f).sum::<u64>() as f64 };
    let kind = if events { "event_cells" } else { "value_cells" };
    let name = format!("contention.{kind}.threads_{threads}");
    m.set(
        &format!("{name}.writes_never_seen_fraction"),
        sum(|l| l.3.load(Relaxed)) / total,
    );
    m.set(
        &format!("{name}.stale_read_after_write_fraction"),
        sum(|l| l.4.load(Relaxed)) / total,
    );
    m.set(
        &format!("{name}.final_value_missing"),
        sum(|l| u64::from(l.2.load(Relaxed) != 20_000)),
    );
    m.set(
        &format!("{name}.writes_per_s"),
        total / (elapsed as f64 / 1e9),
    );
}

fn one_batch(m: &mut Metrics) {
    for (kind, events) in [("value_cell", false), ("event_cell", true)] {
        let build = || {
            let src = Cell::new(0u64);
            let out = src.clone().map(|v| *v).materialize();
            (src, out)
        };
        let (src, out) = if events {
            adapter::events(build)
        } else {
            build()
        };
        let hits = counter();
        let _sink = count(&out, &hits);
        let before = hits.load(Relaxed);
        write(|| {
            for i in 1..=10 {
                src.set(i);
            }
        });
        m.set(
            &format!("one_batch_ten_sets.{kind}.sink_callbacks"),
            (hits.load(Relaxed) - before) as f64,
        );
    }
}

pub fn cost(ctx: &mut Ctx) {
    let m = &mut ctx.m;
    build_costs(m);
    fire_costs(m);
    map_costs(m);
    for n in [100u64, 10_000, 100_000] {
        predicate_fan(m, n, false);
        predicate_fan(m, n, true);
    }
    equal_values(m);
    quantised_tick(m, false);
    quantised_tick(m, true);
    one_batch(m);
    for threads in [1usize, 2, 4, 8] {
        contention(m, threads, false);
        contention(m, threads, true);
    }
}

/// A key cell, a membership cell and `get_value` must read what the
/// collection stores, when the collection was built inside the same batch or
/// callback and when it was dropped before the batch closed. These are the
/// regression tests of two bugs that a later scheduler branch introduced and
/// fixed (d44e316 and 05204bb on `hyphae-settle-core`). 1 means the bug shows.
pub fn backport(ctx: &mut Ctx) {
    let m = &mut ctx.m;
    // d44e316: a key cell made inside a batch, whose map is dropped before
    // the batch closes, must end at the stored value.
    let cell = adapter::write(9, || {
        let map = CellMap::<&'static str, u64>::new();
        map.insert("key", 5);
        map.get(&"key").materialize()
    });
    adapter::flush();
    m.count(
        "viol.collection.d44e316.key_cell_stale_after_map_dropped",
        u64::from(cell.get() != Some(5)),
    );
    // 05204bb's form of the same test: the cell is made before the write.
    let cell = adapter::write(9, || {
        let map = CellMap::<&'static str, u64>::new();
        let cell = map.get(&"key").materialize();
        map.insert("key", 5);
        cell
    });
    adapter::flush();
    m.count(
        "viol.collection.d44e316.key_cell_observed_first_stale",
        u64::from(cell.get() != Some(5)),
    );
    let member = adapter::write(9, || {
        let set = hyphae::CellSet::<&'static str>::new();
        let member = set.contains(&"value").materialize();
        set.insert("value");
        member
    });
    adapter::flush();
    m.count(
        "viol.collection.d44e316.membership_cell_stale",
        u64::from(!member.get()),
    );
    let weak = adapter::write(9, || {
        let map = CellMap::<&'static str, u64>::new();
        map.insert("key", 5);
        map.downgrade()
    });
    adapter::flush();
    m.count(
        "viol.collection.d44e316.map_leaked",
        u64::from(weak.upgrade().is_some()),
    );

    // 05204bb: a key cell made in a subscriber callback must read what a map
    // built in that callback stores. Once with the trigger written bare,
    // once inside a batch.
    for (label, batched) in [("unbatched_trigger", false), ("batched_trigger", true)] {
        let source = Cell::new(0u64);
        let seen = Arc::new(parking_lot::Mutex::new(None));
        let record = Arc::clone(&seen);
        let _guard = source.subscribe(move |signal| {
            if let Signal::Value(value) = signal
                && **value == 1
            {
                let map = CellMap::<&'static str, u64>::new();
                map.insert("key", 5);
                *record.lock() = Some(map.get(&"key").materialize().get());
            }
        });
        if batched {
            adapter::write(9, || source.set(1))
        } else {
            source.set(1)
        }
        adapter::flush();
        m.count(
            &format!("viol.collection.05204bb.key_read_in_callback_wrong.{label}"),
            u64::from(*seen.lock() != Some(Some(5))),
        );
    }
    // The same read with a derived reader inside a batch on the caller's
    // thread, which is how a handler fills a query map and reads it back.
    let read = adapter::write(9, || {
        let map = CellMap::<&'static str, u64>::new();
        map.insert("key", 5);
        (map.get(&"key").materialize().get(), map.get_value(&"key"))
    });
    m.count(
        "viol.collection.05204bb.key_cell_read_in_batch_wrong",
        u64::from(read.0 != Some(5)),
    );
    m.count(
        "viol.collection.05204bb.get_value_in_batch_wrong",
        u64::from(read.1 != Some(5)),
    );
}

/// A diamond whose branch 0 passes the source on and whose other branches
/// carry `source / 4`, so they repeat their value three writes in four. The
/// cell below the join counts its evaluations and checks that every branch
/// reflects the same source value.
struct Diamonds {
    sources: Vec<Src>,
    evals: Arc<AtomicU64>,
    mixed: Arc<AtomicU64>,
    seen: Vec<Arc<AtomicU64>>,
    _keep: Keep,
}

fn quiet<P: Materialize<u64, hyphae::Definite>>(pipeline: P, dedup: bool) -> Node {
    if dedup {
        pipeline.deduped().materialize()
    } else {
        pipeline.materialize()
    }
}

/// `width` branches shared evenly between `sources.len()` sources, joined
/// once, then `nest` more diamonds stacked on the join's output.
fn diamonds(width: usize, writers: usize, nest: usize, dedup: bool) -> Diamonds {
    let sources: Vec<Src> = (0..writers).map(|_| Cell::new(0u64)).collect();
    let (evals, mixed, mut keep) = (counter(), counter(), Keep::new());
    let per = width / writers;
    let branches: Vec<Node> = (0..width)
        .map(|i| {
            let src = sources[i / per].clone();
            if i % per == 0 {
                quiet(src.map(|v| *v), dedup)
            } else {
                quiet(src.map(|v| v / 4), dedup)
            }
        })
        .collect();
    let (e, x) = (Arc::clone(&evals), Arc::clone(&mixed));
    // The join's output: each source's own value, in source order.
    let joined = hyphae::join_vec(branches.clone()).map(move |vals| {
        e.fetch_add(1, Relaxed);
        let mut out = Vec::with_capacity(writers);
        for group in vals.chunks(per) {
            if group[1..].iter().any(|b| *b != group[0] / 4) {
                x.fetch_add(1, Relaxed);
            }
            out.push(group[0]);
        }
        out
    });
    let mut top: Cell<Vec<u64>, CellImmutable> = if dedup {
        joined.deduped().materialize()
    } else {
        joined.materialize()
    };
    for _ in 0..nest {
        let left = top.clone().map(|v| v.clone());
        let right = top
            .clone()
            .map(|v| v.iter().map(|x| x / 4).collect::<Vec<u64>>());
        let (left, right) = if dedup {
            (left.deduped().materialize(), right.deduped().materialize())
        } else {
            (left.materialize(), right.materialize())
        };
        let (e, x) = (Arc::clone(&evals), Arc::clone(&mixed));
        let next = left.clone().join(right.clone()).map(move |v| {
            e.fetch_add(1, Relaxed);
            if v.0.iter().zip(&v.1).any(|(a, b)| *b != a / 4) {
                x.fetch_add(1, Relaxed);
            }
            v.0.clone()
        });
        keep.push(Box::new((top, left, right)));
        top = if dedup {
            next.deduped().materialize()
        } else {
            next.materialize()
        };
    }
    let seen: Vec<Arc<AtomicU64>> = (0..writers).map(|_| counter()).collect();
    let sinks = seen.clone();
    keep.push(Box::new(top.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            for (sink, value) in sinks.iter().zip(v.iter()) {
                sink.store(*value, Relaxed);
            }
        }
    })));
    keep.push(Box::new((branches, top)));
    Diamonds {
        sources,
        evals,
        mixed,
        seen,
        _keep: keep,
    }
}

/// Does a join compute once, on consistent inputs, when some parents stay
/// silent? Widths 2, 8 and 64, three nested diamonds, and two and four
/// concurrent writers, each as built and with `deduped()` on every cell.
/// Run it on a build with dedup forced into `Cell::notify` to test that too.
pub fn diamonds_dedup(ctx: &mut Ctx) {
    let m = &mut ctx.m;
    for dedup in [false, true] {
        let variant = if dedup {
            "deduped_everywhere"
        } else {
            "as_built"
        };
        for (width, nest) in [(2usize, 0usize), (8, 0), (64, 0), (2, 3), (8, 3)] {
            let name = format!("dd.width_{width}.nested_{nest}.{variant}");
            let d = diamonds(width, 1, nest, dedup);
            let src = &d.sources[0];
            let joins = (1 + nest) as f64;
            let mut v = 0u64;
            let mut run = |label: &str, batched: bool, next: &mut dyn FnMut(u64) -> u64| {
                let before = d.evals.load(Relaxed);
                for _ in 0..3_000 {
                    v = next(v);
                    if batched {
                        write(|| src.set(v))
                    } else {
                        src.set(v)
                    }
                }
                m.set(
                    &format!("{name}.join_evals_per_write_per_join.{label}"),
                    (d.evals.load(Relaxed) - before) as f64 / 3_000.0 / joins,
                );
            };
            // v = 4k+1, 4k+2, 4k+3: branch 0 changes, the others repeat.
            run("one_parent_changes", true, &mut |v| {
                if v % 4 == 3 { v + 2 } else { v + 1 }
            });
            run("all_parents_change", true, &mut |v| v + 4);
            run("no_parent_changes", true, &mut |v| v);
            run("one_parent_changes_again", true, &mut |v| {
                if v % 4 == 3 { v + 2 } else { v + 1 }
            });
            let mixed_batched = d.mixed.load(Relaxed);
            run("one_parent_changes.unbatched", false, &mut |v| {
                if v % 4 == 3 { v + 2 } else { v + 1 }
            });
            run("all_parents_change.unbatched", false, &mut |v| v + 4);
            m.count(&format!("{name}.mixed_states.batched"), mixed_batched);
            m.count(
                &format!("{name}.mixed_states.unbatched"),
                d.mixed.load(Relaxed) - mixed_batched,
            );
            m.count(
                &format!("{name}.stalled"),
                u64::from(d.seen[0].load(Relaxed) != v),
            );
        }
        for (width, writers) in [(8usize, 2usize), (64, 4)] {
            let name = format!("dd.width_{width}.writers_{writers}.{variant}");
            let d = diamonds(width, writers, 2, dedup);
            let writes = 20_000u64;
            std::thread::scope(|s| {
                for src in &d.sources {
                    s.spawn(move || {
                        for v in 1..=writes {
                            write(|| src.set(v));
                        }
                    });
                }
            });
            adapter::flush();
            std::thread::sleep(std::time::Duration::from_millis(100));
            m.count(&format!("{name}.mixed_states"), d.mixed.load(Relaxed));
            m.count(
                &format!("{name}.stalled"),
                d.seen.iter().filter(|s| s.load(Relaxed) != writes).count() as u64,
            );
            m.set(
                &format!("{name}.join_evals_per_write"),
                d.evals.load(Relaxed) as f64 / (writes * writers as u64) as f64 / 3.0,
            );
        }
    }
}
