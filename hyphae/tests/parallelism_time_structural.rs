//! Wide-parallel-wave correctness for hyphae's time-based and structural
//! operators under the opt-in `scheduler` feature.
//!
//! # What a "wave-parallel hazard" actually is here
//!
//! The scheduler defers every `notify` inside [`batch`] into a height-ordered
//! queue and drains it one height-wave at a time. Two cells at the *same*
//! height can never depend on each other, so once a wave crosses
//! `PARALLEL_WAVE_THRESHOLD` (8) the scheduler dispatches the whole wave across
//! rayon and its ops run genuinely concurrently. The canonical torn-value
//! hazard that motivates this file's siblings
//! (`scheduler_completeness::join_vec_wide_wave...`) needs a **multi-input
//! combine** whose sink peeks at a *sibling's* state while both sides run in the
//! same wide wave — that's the shape `join`/`join_vec` had to be hardened for.
//!
//! **None of the twelve operators covered here is that shape.** Every one is
//! either single-input (debounce/throttle/delay/timeout/buffer_time/
//! backpressure/cold/finalize/retry/audit/parallel) or a *sequential* two-input
//! composition that never combines its inputs (concat: one input is live at a
//! time). So none is a genuine torn-value / Template-A candidate — there is no
//! sibling state for a concurrent wave to tear. What is still worth pinning
//! down is **robustness under a wide wave**: N independent instances of an
//! operator, driven through one genuinely parallel wave, must each settle on
//! exactly the value their own input dictates, with no cross-instance leak,
//! drop, or corruption of the operator's per-instance state (generation
//! counters, first-skip flags, ArrayQueue/SegQueue buffers, once-callbacks).
//! That is the Template-B correctness harness, and it is what this file writes.
//!
//! # Where the wave does and doesn't reach (per operator)
//!
//! An operator's derived cell only rides the wave if its subscribe callback
//! calls `notify` *synchronously*. Operators that instead arm a platform timer
//! (`spawn_delayed`/`spawn_interval`) settle their output from the shared timer
//! reactor thread, **outside any batch wave** — so a wave-parallel test of their
//! *output* is genuinely N/A. For those we still drive the operator's
//! **input side** through a wide parallel wave (N independent sources set in one
//! batch → a wide height-0 wave running N operator callbacks concurrently) and
//! then observe the timer-driven output settle. Each such test is marked with a
//! `// NOTE:` spelling out what the wave does and does not cover.
//!
//! Classification (see the per-test NOTEs for detail):
//!   - Synchronous output, fully wave-tested:   throttle (leading edge),
//!     timeout (value pass-through), drop_oldest, drop_newest, sample_latest,
//!     concat, cold, finalize, retry, parallel.
//!   - Timer-driven output, input-side wave only: debounce, delay, buffer_time,
//!     audit.
//!   - Genuine torn-value (Template-A) candidates: NONE — all are single-input
//!     or sequential-two-input, documented above.
#![cfg(feature = "scheduler")]

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use hyphae::{
    AuditExt, BackpressureExt, BufferTimeExt, Cell, ColdExt, ConcatExt, DebounceExt, DelayExt,
    FinalizeExt, Gettable, MaterializeDefinite, MaterializeEmpty, Mutable, ParallelExt, RetryExt,
    Signal, ThrottleExt, TimeoutExt, Watchable, batch,
};

/// The scheduler's tick queue is one process-wide structure, so `#[test]` fns
/// (run as concurrent threads by the test harness) would otherwise interleave
/// their batches. Serialize every test in this file through one lock. Copied
/// verbatim from `scheduler_completeness.rs`.
fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    // Force the wave-parallel drain path at test width: production defaults
    // the group threshold high (waves stay sequential at rest), so parallelism
    // tests must lower it to actually exercise concurrent same-height groups.
    hyphae::scheduler::set_wave_threshold_for_test(4);
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Width of every parallel wave below. 16 > PARALLEL_WAVE_THRESHOLD (8), so
/// each wave genuinely dispatches across rayon rather than taking the
/// small-wave sequential fallback.
const WIDE: usize = 16;

/// Poll `settled` until it holds or `deadline` elapses; returns whether it
/// settled. Timer-driven operators emit their output from the shared reactor
/// thread, which a saturated host (e.g. a wave-parallel CI runner) can starve
/// for hundreds of ms — so a single fixed `sleep` then races that starvation
/// and flakes. Polling to a generous deadline keeps the correctness check
/// intact (a genuinely wrong or crossed value never satisfies the predicate and
/// still fails the follow-up assert) while tolerating a merely-late delivery.
fn wait_until(deadline: Duration, mut settled: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if settled() {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Generous deadline for a timer-driven output to settle under a saturated
/// host. Far larger than any operator window used below (≤30 ms), so it only
/// ever absorbs reactor starvation, never masks a missing emit.
const SETTLE_DEADLINE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Time-based operators
// ---------------------------------------------------------------------------

// NOTE: debounce's OUTPUT is timer-driven (`spawn_delayed`), so the derived
// cell never rides the batch wave — a wave-parallel test of the output is N/A.
// What IS wave-parallel here is the INPUT side: N independent sources set in one
// batch form a wide height-0 wave that runs N debounce callbacks concurrently,
// each arming its own generation-stamped timer. This pins that concurrent
// arming doesn't cross-wire generations between independent instances. Distinct
// per-instance values catch any such leak.
#[test]
fn debounce_wide_parallel_input_wave_all_fire() {
    let _serial = scheduler_test_serial();
    let dur = Duration::from_millis(30);

    let sources: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let outs: Vec<_> = sources.iter().map(|s| s.debounce(dur)).collect();

    // One wide batch: all WIDE source ops land at height 0 → parallel wave.
    batch(|| {
        for (i, s) in sources.iter().enumerate() {
            s.set(1000 + i as i64);
        }
    });

    // Output settles from the timer reactor after the debounce quiet period.
    wait_until(SETTLE_DEADLINE, || {
        outs.iter()
            .enumerate()
            .all(|(i, o)| o.get() == 1000 + i as i64)
    });
    for (i, out) in outs.iter().enumerate() {
        assert_eq!(
            out.get(),
            1000 + i as i64,
            "debounce instance {i} settled on the wrong value under a wide input wave"
        );
    }
}

// NOTE: throttle's leading emit is SYNCHRONOUS (`c.notify` runs inside the
// subscribe callback; the timer only re-opens the gate later), so the derived
// cell DOES ride the wave — this is a genuine wave-parallel output test. The
// construction-time replay consumes each instance's `can_emit` token, so we let
// the per-instance reset timers fire before each wide-wave set.
#[test]
fn throttle_wide_parallel_wave_leading_emit_correct() {
    let _serial = scheduler_test_serial();
    let dur = Duration::from_millis(20);

    let sources: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let outs: Vec<_> = sources.iter().map(|s| s.throttle(dur)).collect();

    for round in 0..3i64 {
        // Let every instance's `can_emit` reset before the next leading edge.
        thread::sleep(Duration::from_millis(60));
        let base = (round + 1) * 1000;
        batch(|| {
            for (i, s) in sources.iter().enumerate() {
                s.set(base + i as i64);
            }
        });
        for (i, out) in outs.iter().enumerate() {
            assert_eq!(
                out.get(),
                base + i as i64,
                "throttle instance {i} leading-edge emit wrong under a wide wave (round {round})"
            );
        }
    }
}

// NOTE: delay's output is timer-driven (`spawn_delayed` on every signal), so
// its output is out of the wave path — N/A. Wide-wave coverage is on the input
// side: N independent sources set in one batch run N delay callbacks
// concurrently, each scheduling its own delivery. Assert the delayed values all
// arrive intact and uncrossed.
#[test]
fn delay_wide_parallel_input_wave_all_fire() {
    let _serial = scheduler_test_serial();
    let dur = Duration::from_millis(30);

    let sources: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let outs: Vec<_> = sources.iter().map(|s| s.delay(dur)).collect();

    batch(|| {
        for (i, s) in sources.iter().enumerate() {
            s.set(2000 + i as i64);
        }
    });

    wait_until(SETTLE_DEADLINE, || {
        outs.iter()
            .enumerate()
            .all(|(i, o)| o.get() == 2000 + i as i64)
    });
    for (i, out) in outs.iter().enumerate() {
        assert_eq!(
            out.get(),
            2000 + i as i64,
            "delay instance {i} delivered the wrong value under a wide input wave"
        );
    }
}

// NOTE: timeout's value pass-through is SYNCHRONOUS (`d.notify(Value)` inside
// the callback), so the derived cell rides the wave — this is a genuine
// wave-parallel output test of the value path. The timeout-ERROR branch is the
// only timer-driven part and is orthogonal to the wave; a generous window keeps
// it from firing while we drive values. The loop stays well under that window.
#[test]
fn timeout_wide_parallel_wave_value_passthrough_correct() {
    let _serial = scheduler_test_serial();
    const ITERS: i64 = 100;
    let dur = Duration::from_millis(500);

    let sources: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let outs: Vec<_> = sources.iter().map(|s| s.timeout(dur)).collect();

    for it in 1..=ITERS {
        batch(|| {
            for (i, s) in sources.iter().enumerate() {
                s.set(it * 1000 + i as i64);
            }
        });
        for (i, out) in outs.iter().enumerate() {
            assert_eq!(
                out.get(),
                it * 1000 + i as i64,
                "timeout instance {i} passed a torn/stale value under a wide wave (iter {it})"
            );
        }
    }
}

// NOTE: buffer_time's output is timer-driven (`spawn_interval`), so the output
// is out of the wave path — N/A. Wide-wave coverage is on the input side: N
// independent sources push into N independent SegQueues concurrently in one wide
// height-0 wave. Each instance pushes exactly one value, so the flattened union
// of that instance's window emissions must be exactly its one value — no push
// lost, none leaked to a sibling buffer.
#[test]
fn buffer_time_wide_parallel_input_wave_all_collect() {
    let _serial = scheduler_test_serial();
    let dur = Duration::from_millis(30);

    let sources: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let outs: Vec<_> = sources.iter().map(|s| s.buffer_time(dur)).collect();

    // Accumulate every non-empty flattened emission per instance.
    let seen: Vec<Arc<Mutex<Vec<i64>>>> = (0..WIDE)
        .map(|_| Arc::new(Mutex::new(Vec::new())))
        .collect();
    let mut guards = Vec::new();
    for (i, out) in outs.iter().enumerate() {
        let sink = seen[i].clone();
        guards.push(out.subscribe(move |sig| {
            if let Signal::Value(v) = sig {
                sink.lock().unwrap().extend(v.iter().copied());
            }
        }));
    }

    batch(|| {
        for (i, s) in sources.iter().enumerate() {
            s.set(3000 + i as i64);
        }
    });

    // Let a couple of windows elapse so the pushed value is flushed.
    wait_until(SETTLE_DEADLINE, || {
        seen.iter()
            .enumerate()
            .all(|(i, s)| *s.lock().unwrap() == vec![3000 + i as i64])
    });
    for (i, s) in seen.iter().enumerate() {
        assert_eq!(
            *s.lock().unwrap(),
            vec![3000 + i as i64],
            "buffer_time instance {i} lost/leaked its buffered value under a wide input wave"
        );
    }
    drop(guards);
}

// NOTE: audit's output is timer-driven (`spawn_delayed` at window open) and it
// samples the LAST value seen in the window via a per-instance Mutex — but that
// Mutex is never shared across instances, so there is no cross-instance combine
// to tear. Output is out of the wave path — N/A. Wide-wave coverage is on the
// input side: N independent sources each open a window concurrently in one wide
// height-0 wave; each must emit its own last value.
#[test]
fn audit_wide_parallel_input_wave_all_fire() {
    let _serial = scheduler_test_serial();
    let dur = Duration::from_millis(30);

    let sources: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let outs: Vec<_> = sources.iter().map(|s| s.audit(dur)).collect();

    batch(|| {
        for (i, s) in sources.iter().enumerate() {
            s.set(4000 + i as i64);
        }
    });

    wait_until(SETTLE_DEADLINE, || {
        outs.iter()
            .enumerate()
            .all(|(i, o)| o.get() == 4000 + i as i64)
    });
    for (i, out) in outs.iter().enumerate() {
        assert_eq!(
            out.get(),
            4000 + i as i64,
            "audit instance {i} sampled the wrong last value under a wide input wave"
        );
    }
}

// ---------------------------------------------------------------------------
// Backpressure operators (synchronous pass-through — full wave-parallel output)
// ---------------------------------------------------------------------------

// NOTE: drop_oldest emits synchronously on every value, so its derived cell
// rides the wave. Capacity is kept above the single per-batch push so no drop
// occurs and the settled value is exactly deterministic. Genuine wave-parallel
// output test across WIDE independent instances.
#[test]
fn backpressure_drop_oldest_wide_parallel_wave_correct() {
    let _serial = scheduler_test_serial();
    const ITERS: i64 = 200;

    let sources: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let outs: Vec<_> = sources.iter().map(|s| s.drop_oldest(8)).collect();

    for it in 1..=ITERS {
        batch(|| {
            for (i, s) in sources.iter().enumerate() {
                s.set(it * 1000 + i as i64);
            }
        });
        for (i, out) in outs.iter().enumerate() {
            assert_eq!(
                out.get(),
                it * 1000 + i as i64,
                "drop_oldest instance {i} wrong under a wide wave (iter {it})"
            );
        }
    }
}

// NOTE: drop_newest emits synchronously when the buffer has room. Capacity is
// kept above the single per-batch push so the value is never dropped and the
// settled value is deterministic. Genuine wave-parallel output test.
#[test]
fn backpressure_drop_newest_wide_parallel_wave_correct() {
    let _serial = scheduler_test_serial();
    const ITERS: i64 = 200;

    const CAP: i64 = 8;
    let sources: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let outs: Vec<_> = sources
        .iter()
        .map(|s| s.drop_newest(CAP as usize))
        .collect();

    for it in 1..=ITERS {
        batch(|| {
            for (i, s) in sources.iter().enumerate() {
                s.set(it * 1000 + i as i64);
            }
        });
        for (i, out) in outs.iter().enumerate() {
            // drop_newest's ArrayQueue(CAP) is never drained (see the operator's
            // own `test_drop_newest`): after CAP accepted values it is full and
            // every later value is dropped with no emission. So each instance
            // passes values through for its first CAP iterations, then freezes at
            // its CAP-th value. The wide-wave property under test is that no
            // instance is corrupted by a concurrent sibling — each output only
            // ever holds ITS OWN source's values, frozen at exactly the right one.
            let expected = it.min(CAP) * 1000 + i as i64;
            assert_eq!(
                out.get(),
                expected,
                "drop_newest instance {i} wrong under a wide wave (iter {it})"
            );
        }
    }
}

// NOTE: sample_latest is a synchronous latest-value pass-through, so its derived
// cell rides the wave. Genuine wave-parallel output test.
#[test]
fn backpressure_sample_latest_wide_parallel_wave_correct() {
    let _serial = scheduler_test_serial();
    const ITERS: i64 = 200;

    let sources: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let outs: Vec<_> = sources.iter().map(|s| s.sample_latest()).collect();

    for it in 1..=ITERS {
        batch(|| {
            for (i, s) in sources.iter().enumerate() {
                s.set(it * 1000 + i as i64);
            }
        });
        for (i, out) in outs.iter().enumerate() {
            assert_eq!(
                out.get(),
                it * 1000 + i as i64,
                "sample_latest instance {i} wrong under a wide wave (iter {it})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Structural operators
// ---------------------------------------------------------------------------

// NOTE: concat is a SEQUENTIAL two-input composition — only one input is live at
// a time, so it never *combines* the two and has no sibling state for a wave to
// tear (not a Template-A candidate). Its output forwards synchronously, so it
// rides the wave. This drives BOTH input sides under a wide parallel wave with
// the completion-driven hand-off in between, checking the per-instance
// first_skip/first_done/second_skip state survives concurrent settling.
#[test]
fn concat_wide_parallel_wave_both_sides_correct() {
    let _serial = scheduler_test_serial();

    let firsts: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let seconds: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let outs: Vec<_> = firsts
        .iter()
        .zip(seconds.iter())
        .map(|(f, s)| f.concat(s))
        .collect();

    // Phase 1: drive the FIRST input side under a wide wave.
    batch(|| {
        for (i, f) in firsts.iter().enumerate() {
            f.set(5000 + i as i64);
        }
    });
    for (i, out) in outs.iter().enumerate() {
        assert_eq!(
            out.get(),
            5000 + i as i64,
            "concat instance {i} wrong on the first-input side under a wide wave"
        );
    }

    // Hand off to the second input by completing every first source. Done
    // outside a batch so each second subscription is established synchronously.
    for f in &firsts {
        f.complete();
    }

    // Phase 2: drive the SECOND input side under a wide wave.
    batch(|| {
        for (i, s) in seconds.iter().enumerate() {
            s.set(6000 + i as i64);
        }
    });
    for (i, out) in outs.iter().enumerate() {
        assert_eq!(
            out.get(),
            6000 + i as i64,
            "concat instance {i} wrong on the second-input side after the completion hand-off"
        );
    }
}

// NOTE: cold is a single-input pipeline operator that swallows the first
// (retained) emission and lifts subsequent ones to Some(Arc<_>). Output forwards
// synchronously → rides the wave. Genuine wide-wave output test: each cold cell
// starts None and settles Some(value) after the first post-subscribe emission.
#[test]
fn cold_wide_parallel_wave_settles_some() {
    let _serial = scheduler_test_serial();
    const ITERS: i64 = 200;

    let sources: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let outs: Vec<_> = sources
        .iter()
        .map(|s| s.clone().cold().materialize())
        .collect();

    // First-skip is consumed at construction (synchronous replay), so each
    // cold cell reads None before any batch.
    for (i, out) in outs.iter().enumerate() {
        assert_eq!(out.get(), None, "cold instance {i} should start None");
    }

    for it in 1..=ITERS {
        batch(|| {
            for (i, s) in sources.iter().enumerate() {
                s.set(it * 1000 + i as i64);
            }
        });
        for (i, out) in outs.iter().enumerate() {
            assert_eq!(
                out.get(),
                Some(Arc::new(it * 1000 + i as i64)),
                "cold instance {i} settled wrong under a wide wave (iter {it})"
            );
        }
    }
}

// NOTE: finalize is a single-input pass-through with a once-only terminal
// callback. Values forward synchronously → the value path rides the wave. This
// checks value pass-through under a wide wave AND that each instance's
// OnceCallback fires exactly once on completion (the terminal path is not itself
// wave-parallel, but its once-guard is exercised across all instances).
#[test]
fn finalize_wide_parallel_wave_passthrough_and_terminal() {
    let _serial = scheduler_test_serial();
    const ITERS: i64 = 200;

    let flags: Vec<Arc<AtomicI64>> = (0..WIDE).map(|_| Arc::new(AtomicI64::new(0))).collect();
    let sources: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let outs: Vec<_> = sources
        .iter()
        .zip(flags.iter())
        .map(|(s, flag)| {
            let flag = flag.clone();
            s.clone()
                .finalize(move || {
                    flag.fetch_add(1, Ordering::SeqCst);
                })
                .materialize()
        })
        .collect();

    for it in 1..=ITERS {
        batch(|| {
            for (i, s) in sources.iter().enumerate() {
                s.set(it * 1000 + i as i64);
            }
        });
        for (i, out) in outs.iter().enumerate() {
            assert_eq!(
                out.get(),
                it * 1000 + i as i64,
                "finalize instance {i} passed a wrong value under a wide wave (iter {it})"
            );
        }
    }

    // Terminal: complete every source; each finalize callback fires exactly once.
    for s in &sources {
        s.complete();
    }
    for (i, flag) in flags.iter().enumerate() {
        assert_eq!(
            flag.load(Ordering::SeqCst),
            1,
            "finalize instance {i} terminal callback did not fire exactly once"
        );
    }
}

// NOTE: retry is single-input; on a value it passes through synchronously (the
// retry/resubscribe machinery only engages on Error, which is orthogonal to the
// wave). Value path rides the wave → genuine wide-wave output test of
// pass-through with a high attempt budget so no error path is taken.
#[test]
fn retry_wide_parallel_wave_value_passthrough() {
    let _serial = scheduler_test_serial();
    const ITERS: i64 = 200;

    let sources: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let outs: Vec<_> = sources.iter().map(|s| s.retry(1_000)).collect();

    for it in 1..=ITERS {
        batch(|| {
            for (i, s) in sources.iter().enumerate() {
                s.set(it * 1000 + i as i64);
            }
        });
        for (i, out) in outs.iter().enumerate() {
            assert_eq!(
                out.get(),
                it * 1000 + i as i64,
                "retry instance {i} passed a wrong value under a wide wave (iter {it})"
            );
        }
    }
}

// NOTE: `parallel()` fans OUT to its own subscribers via rayon directly, inside
// the source op — it bypasses the scheduler queue for its fan-out. Driving WIDE
// independent sources in one batch makes a wide height-0 wave (itself on rayon)
// whose ops each run parallel's nested rayon fan-out: rayon-within-rayon. This
// checks that nested parallel dispatch delivers each source's value to its own
// subscriber intact, with no cross-instance leak. Not a torn-value candidate
// (single input, per-instance subscriber set).
#[test]
fn parallel_wide_parallel_input_wave_correct() {
    let _serial = scheduler_test_serial();
    const ITERS: i64 = 200;

    let sources: Vec<Cell<i64, _>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let cells: Vec<_> = sources.iter().map(|s| s.parallel()).collect();

    let slots: Vec<Arc<AtomicI64>> = (0..WIDE)
        .map(|_| Arc::new(AtomicI64::new(i64::MIN)))
        .collect();
    let mut guards = Vec::new();
    for (i, cell) in cells.iter().enumerate() {
        let slot = slots[i].clone();
        guards.push(cell.subscribe(move |sig| {
            if let Signal::Value(v) = sig {
                slot.store(**v, Ordering::SeqCst);
            }
        }));
    }

    for it in 1..=ITERS {
        batch(|| {
            for (i, s) in sources.iter().enumerate() {
                s.set(it * 1000 + i as i64);
            }
        });
        // parallel's fan-out is synchronous within the wave op, so all slots
        // are settled by the time the outermost same-thread batch returns.
        for (i, slot) in slots.iter().enumerate() {
            assert_eq!(
                slot.load(Ordering::SeqCst),
                it * 1000 + i as i64,
                "parallel instance {i} delivered a wrong value under a wide wave (iter {it})"
            );
        }
    }
    drop(guards);
}
