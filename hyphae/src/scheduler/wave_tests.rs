use std::{
    env,
    process::Command,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
        mpsc::sync_channel,
    },
    time::Duration,
};

use parking_lot::Mutex;

use super::*;

static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
const ISOLATED_TEST: &str = "HYPHAE_SCHEDULER_ISOLATED_TEST";

fn run_isolated(name: &str) -> bool {
    let test_name = format!("scheduler::wave_tests::{name}");
    if env::var(ISOLATED_TEST).is_ok_and(|selected| selected == test_name) {
        return true;
    }
    let status = env::current_exe().ok().and_then(|executable| {
        Command::new(executable)
            .args(["--exact", &test_name, "--nocapture", "--test-threads=1"])
            .env(ISOLATED_TEST, &test_name)
            .status()
            .ok()
    });
    assert!(status.is_some_and(|status| status.success()));
    false
}

struct TestNode {
    id: Uuid,
    no_coalesce: bool,
    deps: Vec<Arc<dyn DepNode>>,
}

struct DropCapture(Arc<AtomicUsize>);

impl Drop for DropCapture {
    fn drop(&mut self) {
        self.0.fetch_add(1, AtomicOrdering::Relaxed);
    }
}

struct ReenterSchedulerOnDrop(Arc<AtomicUsize>);

impl Drop for ReenterSchedulerOnDrop {
    fn drop(&mut self) {
        batch(|| {});
        self.0.fetch_add(1, AtomicOrdering::Relaxed);
    }
}

struct PanicOnDrop(Arc<AtomicUsize>);

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, AtomicOrdering::Relaxed);
        std::panic::resume_unwind(Box::new("discarded callback drop panic"));
    }
}

impl TestNode {
    fn source(id: u128) -> Self {
        Self {
            id: Uuid::from_u128(id),
            no_coalesce: false,
            deps: Vec::new(),
        }
    }
}

impl DepNode for TestNode {
    fn id(&self) -> Uuid {
        self.id
    }

    fn name(&self) -> Option<String> {
        None
    }

    fn deps(&self) -> Vec<Arc<dyn DepNode>> {
        self.deps.clone()
    }

    fn no_coalesce(&self) -> bool {
        self.no_coalesce
    }
}

fn empty_tick() -> SharedTick {
    SharedTick {
        order: BTreeMap::new(),
        scheduled: FxHashMap::default(),
        seq: 0,
        parallel_wave_active: false,
        depth: 0,
        draining: false,
    }
}

fn tagged_run(tag: u64, seen: Arc<Mutex<Vec<u64>>>) -> DeferredOp {
    Box::new(move || seen.lock().push(tag))
}

fn deferred(run: impl FnOnce() + Send + 'static) -> DeferredOp {
    Box::new(run)
}

fn drain_local(tick: &mut SharedTick) {
    while let Some((_, run)) = tick.order.pop_first() {
        run();
    }
}

fn reset_global_tick(depth: u32, draining: bool) {
    let mut tick = TICK.lock();
    tick.order.clear();
    tick.scheduled.clear();
    tick.seq = 0;
    tick.parallel_wave_active = false;
    NEXT_SEQ.store(0, Ordering::Relaxed);
    tick.depth = depth;
    tick.draining = draining;
    refresh_tick_active(&tick);
    drop(tick);
}

fn drain_global_order() {
    let mut runs = Vec::new();
    {
        let mut tick = TICK.lock();
        while let Some((_, run)) = tick.order.pop_first() {
            runs.push(run);
        }
        tick.scheduled.clear();
        drop(tick);
    }
    for run in runs {
        run();
    }
}

#[test]
fn parallel_sequence_phase_preserves_both_boundaries() {
    if !run_isolated("parallel_sequence_phase_preserves_both_boundaries") {
        return;
    }
    let _test = TEST_LOCK.lock();
    reset_global_tick(1, true);
    TICK.lock().seq = 40;
    let node = Arc::new(TestNode {
        id: Uuid::from_u128(40),
        no_coalesce: true,
        deps: Vec::new(),
    });
    let seen = Arc::new(Mutex::new(Vec::new()));

    enqueue(
        node.id,
        node.as_ref(),
        false,
        tagged_run(0, Arc::clone(&seen)),
    );
    let phase = WaveSequencePhase::begin();
    let scope = PendingScope::enter();
    enqueue(
        node.id,
        node.as_ref(),
        false,
        tagged_run(1, Arc::clone(&seen)),
    );
    let pending = scope.finish();

    let external_node = Arc::clone(&node);
    let external_seen = Arc::clone(&seen);
    let external = std::thread::spawn(move || {
        enqueue(
            external_node.id,
            external_node.as_ref(),
            false,
            tagged_run(2, external_seen),
        );
    });
    assert!(external.join().is_ok());
    let panics = merge_wave(
        vec![GroupResult {
            pending,
            panics: Vec::new(),
        }],
        phase,
    );
    assert!(panics.is_empty());

    enqueue(
        node.id,
        node.as_ref(),
        false,
        tagged_run(3, Arc::clone(&seen)),
    );
    {
        let tick = TICK.lock();
        assert_eq!(tick.seq, 44);
        assert!(!tick.parallel_wave_active);
        assert_eq!(tick.order.len(), 4);
        drop(tick);
    }
    drain_global_order();
    assert_eq!(*seen.lock(), vec![0, 1, 2, 3]);
    reset_global_tick(0, false);
}

#[test]
fn dropped_parallel_sequence_phase_restores_locked_counter() {
    if !run_isolated("dropped_parallel_sequence_phase_restores_locked_counter") {
        return;
    }
    let _test = TEST_LOCK.lock();
    reset_global_tick(1, true);
    TICK.lock().seq = 70;
    let phase = WaveSequencePhase::begin();
    assert_eq!(NEXT_SEQ.fetch_add(1, Ordering::Relaxed), 70);
    drop(phase);

    let mut tick = TICK.lock();
    assert!(!tick.parallel_wave_active);
    assert_eq!(tick.seq, 71);
    assert_eq!(next_seq_locked(&mut tick), 71);
    assert_eq!(tick.seq, 72);
    drop(tick);
    reset_global_tick(0, false);
}

#[test]
fn callback_panic_preserves_sequence_across_parallel_phases() {
    if !run_isolated("callback_panic_preserves_sequence_across_parallel_phases") {
        return;
    }
    let _test = TEST_LOCK.lock();
    reset_global_tick(1, true);
    TICK.lock().seq = 100;
    let previous_threshold = wave_threshold();
    WAVE_THRESHOLD.store(1, Ordering::Relaxed);
    let node = Arc::new(TestNode {
        id: Uuid::from_u128(41),
        no_coalesce: true,
        deps: Vec::new(),
    });
    let seen = Arc::new(Mutex::new(Vec::new()));

    let first_groups = vec![
        vec![{
            let node = Arc::clone(&node);
            let seen = Arc::clone(&seen);
            deferred(move || -> () {
                enqueue(node.id, node.as_ref(), false, tagged_run(1, seen));
                std::panic::resume_unwind(Box::new("expected wave panic"));
            })
        }],
        vec![{
            let node = Arc::clone(&node);
            let seen = Arc::clone(&seen);
            deferred(move || {
                enqueue(node.id, node.as_ref(), false, tagged_run(2, seen));
            })
        }],
    ];
    assert_eq!(run_wave(first_groups).len(), 1);
    {
        let tick = TICK.lock();
        assert!(!tick.parallel_wave_active);
        assert_eq!(tick.seq, 102);
        drop(tick);
    }

    enqueue(
        node.id,
        node.as_ref(),
        false,
        tagged_run(3, Arc::clone(&seen)),
    );
    let second_groups = vec![vec![{
        let node = Arc::clone(&node);
        let seen = Arc::clone(&seen);
        deferred(move || enqueue(node.id, node.as_ref(), false, tagged_run(4, seen)))
    }]];
    assert!(run_wave(second_groups).is_empty());
    {
        let tick = TICK.lock();
        assert!(!tick.parallel_wave_active);
        assert_eq!(tick.seq, 104);
        assert_eq!(
            tick.order
                .keys()
                .map(|&(_, _, seq)| seq)
                .collect::<Vec<_>>(),
            vec![100, 101, 102, 103]
        );
        drop(tick);
    }

    drain_global_order();
    let mut delivered = seen.lock().clone();
    assert_eq!(delivered.split_off(2), vec![3, 4]);
    delivered.sort_unstable();
    assert_eq!(delivered, vec![1, 2]);
    WAVE_THRESHOLD.store(previous_threshold, Ordering::Relaxed);
    reset_global_tick(0, false);
}

#[test]
fn stamp_aware_merge_rejects_older_coalescing_op() {
    let _test = TEST_LOCK.lock();
    let id = Uuid::from_u128(1);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut tick = empty_tick();

    drop(tick.enqueue_locked(id, 0, 11, true, tagged_run(11, Arc::clone(&seen))));
    drop(tick.enqueue_locked(id, 0, 10, true, tagged_run(10, Arc::clone(&seen))));
    drain_local(&mut tick);

    assert_eq!(*seen.lock(), vec![11]);
}

#[test]
fn stamp_aware_merge_accepts_newer_coalescing_op() {
    let _test = TEST_LOCK.lock();
    let id = Uuid::from_u128(2);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut tick = empty_tick();

    drop(tick.enqueue_locked(id, 0, 10, true, tagged_run(10, Arc::clone(&seen))));
    drop(tick.enqueue_locked(id, 0, 11, true, tagged_run(11, Arc::clone(&seen))));
    drain_local(&mut tick);

    assert_eq!(*seen.lock(), vec![11]);
}

#[test]
fn external_newer_stamp_wins_over_later_physical_local_merge() {
    if !run_isolated("external_newer_stamp_wins_over_later_physical_local_merge") {
        return;
    }
    let _test = TEST_LOCK.lock();
    reset_global_tick(1, true);
    ENQUEUE_LOCK_ACQUISITIONS.store(0, Ordering::Relaxed);
    let node = Arc::new(TestNode::source(20));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let phase = WaveSequencePhase::begin();
    let scope = PendingScope::enter();
    enqueue(
        node.id,
        node.as_ref(),
        false,
        tagged_run(1, Arc::clone(&seen)),
    );
    let pending = scope.finish();
    let (release, start) = sync_channel(0);
    let external_node = Arc::clone(&node);
    let external_seen = Arc::clone(&seen);
    let external = std::thread::spawn(move || {
        if start.recv().is_ok() {
            enqueue(
                external_node.id,
                external_node.as_ref(),
                false,
                tagged_run(2, external_seen),
            );
        }
    });
    assert!(release.send(()).is_ok());
    assert!(external.join().is_ok());
    assert_eq!(ENQUEUE_LOCK_ACQUISITIONS.load(Ordering::Relaxed), 1);

    let panics = merge_wave(
        vec![GroupResult {
            pending,
            panics: Vec::new(),
        }],
        phase,
    );
    assert!(panics.is_empty());
    drain_global_order();
    assert_eq!(*seen.lock(), vec![2]);
    reset_global_tick(0, false);
}

#[test]
fn local_newer_stamp_replaces_external_queued_op() {
    if !run_isolated("local_newer_stamp_replaces_external_queued_op") {
        return;
    }
    let _test = TEST_LOCK.lock();
    reset_global_tick(1, true);
    let node = Arc::new(TestNode::source(21));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let phase = WaveSequencePhase::begin();
    let (release, start) = sync_channel(0);
    let external_node = Arc::clone(&node);
    let external_seen = Arc::clone(&seen);
    let external = std::thread::spawn(move || {
        if start.recv().is_ok() {
            enqueue(
                external_node.id,
                external_node.as_ref(),
                false,
                tagged_run(1, external_seen),
            );
        }
    });
    assert!(release.send(()).is_ok());
    assert!(external.join().is_ok());
    let scope = PendingScope::enter();
    enqueue(
        node.id,
        node.as_ref(),
        false,
        tagged_run(2, Arc::clone(&seen)),
    );

    let panics = merge_wave(
        vec![GroupResult {
            pending: scope.finish(),
            panics: Vec::new(),
        }],
        phase,
    );
    assert!(panics.is_empty());
    drain_global_order();
    assert_eq!(*seen.lock(), vec![2]);
    reset_global_tick(0, false);
}

#[test]
fn lower_height_external_op_runs_before_buffered_higher_height_op() {
    if !run_isolated("lower_height_external_op_runs_before_buffered_higher_height_op") {
        return;
    }
    let _test = TEST_LOCK.lock();
    reset_global_tick(1, true);
    let source: Arc<dyn DepNode> = Arc::new(TestNode::source(26));
    let higher = Arc::new(TestNode {
        id: Uuid::from_u128(27),
        no_coalesce: false,
        deps: vec![Arc::clone(&source)],
    });
    let seen = Arc::new(Mutex::new(Vec::new()));
    let phase = WaveSequencePhase::begin();
    let buffered = run_group_buffered(vec![{
        let higher = Arc::clone(&higher);
        let seen = Arc::clone(&seen);
        deferred(move || {
            enqueue(higher.id, higher.as_ref(), false, tagged_run(2, seen));
        })
    }]);
    enqueue(
        source.id(),
        source.as_ref(),
        false,
        tagged_run(1, Arc::clone(&seen)),
    );

    let panics = merge_wave(vec![buffered], phase);
    assert!(panics.is_empty());
    drain_global_order();
    assert_eq!(*seen.lock(), vec![1, 2]);
    reset_global_tick(0, false);
}

#[test]
fn multiple_worker_buffers_keep_the_value_with_the_greatest_stamp() {
    if !run_isolated("multiple_worker_buffers_keep_the_value_with_the_greatest_stamp") {
        return;
    }
    let _test = TEST_LOCK.lock();
    reset_global_tick(1, true);
    let previous_threshold = wave_threshold();
    WAVE_THRESHOLD.store(1, Ordering::Relaxed);
    let id = Uuid::from_u128(28);
    let stamps = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let groups = (0u64..32)
        .map(|tag| {
            let stamps = Arc::clone(&stamps);
            let seen = Arc::clone(&seen);
            vec![deferred(move || {
                let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
                stamps.lock().push((seq, tag));
                let pending = PendingOp {
                    id,
                    height: 0,
                    seq,
                    coalesce: true,
                    run: tagged_run(tag, seen),
                };
                assert!(push_group_pending(pending).is_ok());
            })]
        })
        .collect();

    let panics = run_wave(groups);
    assert!(panics.is_empty());
    let expected = stamps
        .lock()
        .iter()
        .copied()
        .max_by_key(|(seq, _)| *seq)
        .map(|(_, tag)| tag);
    drain_global_order();
    assert_eq!(seen.lock().first().copied(), expected);
    assert_eq!(seen.lock().len(), 1);
    WAVE_THRESHOLD.store(previous_threshold, Ordering::Relaxed);
    reset_global_tick(0, false);
}

#[test]
fn terminal_and_event_ops_survive_in_stamp_order() {
    let _test = TEST_LOCK.lock();
    let id = Uuid::from_u128(4);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut tick = empty_tick();

    drop(tick.enqueue_locked(id, 0, 21, false, tagged_run(21, Arc::clone(&seen))));
    drop(tick.enqueue_locked(id, 0, 20, false, tagged_run(20, Arc::clone(&seen))));
    drop(tick.enqueue_locked(id, 0, 22, false, tagged_run(22, Arc::clone(&seen))));
    drain_local(&mut tick);

    assert_eq!(*seen.lock(), vec![20, 21, 22]);
}

#[test]
fn older_buffered_value_survives_terminal_merged_first() {
    let _test = TEST_LOCK.lock();
    let id = Uuid::from_u128(29);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut tick = empty_tick();

    drop(tick.enqueue_locked(id, 0, 11, false, tagged_run(11, Arc::clone(&seen))));
    drop(tick.enqueue_locked(id, 0, 10, true, tagged_run(10, Arc::clone(&seen))));
    drain_local(&mut tick);

    assert_eq!(*seen.lock(), vec![10, 11]);
}

#[test]
fn reversed_worker_buffers_and_external_event_keep_global_stamp_order() {
    if !run_isolated("reversed_worker_buffers_and_external_event_keep_global_stamp_order") {
        return;
    }
    let _test = TEST_LOCK.lock();
    reset_global_tick(1, true);
    let node = Arc::new(TestNode {
        id: Uuid::from_u128(22),
        no_coalesce: true,
        deps: Vec::new(),
    });
    let seen = Arc::new(Mutex::new(Vec::new()));
    let phase = WaveSequencePhase::begin();
    let first = run_group_buffered(vec![{
        let node = Arc::clone(&node);
        let seen = Arc::clone(&seen);
        deferred(move || {
            enqueue(node.id, node.as_ref(), false, tagged_run(1, seen));
        })
    }]);
    enqueue(
        node.id,
        node.as_ref(),
        false,
        tagged_run(2, Arc::clone(&seen)),
    );
    let third = run_group_buffered(vec![{
        let node = Arc::clone(&node);
        let seen = Arc::clone(&seen);
        deferred(move || {
            enqueue(node.id, node.as_ref(), false, tagged_run(3, seen));
        })
    }]);

    let panics = merge_wave(vec![third, first], phase);
    assert!(panics.is_empty());
    drain_global_order();
    assert_eq!(*seen.lock(), vec![1, 2, 3]);
    reset_global_tick(0, false);
}

#[test]
fn group_scope_retains_enqueues_after_panic_and_restores_tls() {
    let _test = TEST_LOCK.lock();
    GROUP_PENDING.with(|pending| pending.replace(None));
    let first = Arc::new(TestNode::source(5));
    let second = Arc::new(TestNode::source(6));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let group = vec![
        {
            let first = Arc::clone(&first);
            let seen = Arc::clone(&seen);
            deferred(move || -> () {
                enqueue(
                    first.id,
                    first.as_ref(),
                    false,
                    tagged_run(1, Arc::clone(&seen)),
                );
                std::panic::resume_unwind(Box::new("expected panic"));
            })
        },
        {
            let second = Arc::clone(&second);
            let seen = Arc::clone(&seen);
            deferred(move || {
                enqueue(
                    second.id,
                    second.as_ref(),
                    false,
                    tagged_run(2, Arc::clone(&seen)),
                );
            })
        },
    ];

    let result = run_group_buffered(group);

    assert_eq!(result.panics.len(), 1);
    assert_eq!(result.pending.len(), 2);
    assert!(GROUP_PENDING.with(|pending| pending.borrow().is_none()));
    let mut tick = empty_tick();
    for pending in result.pending {
        drop(tick.enqueue_locked(
            pending.id,
            pending.height,
            pending.seq,
            pending.coalesce,
            pending.run,
        ));
    }
    drain_local(&mut tick);
    assert_eq!(*seen.lock(), vec![1, 2]);
}

#[test]
fn fold_accumulator_preserves_pending_panics_and_outer_tls_between_groups() {
    let _test = TEST_LOCK.lock();
    GROUP_PENDING.with(|pending| pending.replace(None));
    let outer = PendingScope::enter();
    let first = Arc::new(TestNode::source(30));
    let second = Arc::new(TestNode::source(31));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let first_group = vec![{
        let first = Arc::clone(&first);
        let seen = Arc::clone(&seen);
        deferred(move || -> () {
            enqueue(first.id, first.as_ref(), false, tagged_run(1, seen));
            std::panic::resume_unwind(Box::new("fold panic"));
        })
    }];

    let result = run_group_buffered_into(GroupResult::default(), first_group);
    assert_eq!(result.pending.len(), 1);
    assert_eq!(result.panics.len(), 1);
    assert!(GROUP_PENDING.with(|pending| pending.borrow().is_some()));

    let second_group = vec![{
        let second = Arc::clone(&second);
        let seen = Arc::clone(&seen);
        deferred(move || enqueue(second.id, second.as_ref(), false, tagged_run(2, seen)))
    }];
    let result = run_group_buffered_into(result, second_group);
    assert_eq!(result.pending.len(), 2);
    assert_eq!(result.panics.len(), 1);
    assert!(GROUP_PENDING.with(|pending| pending.borrow().is_some()));

    enqueue(first.id, first.as_ref(), false, Box::new(|| {}));
    let outer_pending = outer.finish();
    assert_eq!(outer_pending.len(), 1);
    assert!(GROUP_PENDING.with(|pending| pending.borrow().is_none()));

    let mut tick = empty_tick();
    for pending in result.pending {
        drop(tick.enqueue_locked(
            pending.id,
            pending.height,
            pending.seq,
            pending.coalesce,
            pending.run,
        ));
    }
    drain_local(&mut tick);
    assert_eq!(*seen.lock(), vec![1, 2]);
}

#[test]
fn spare_pending_pool_reuses_empty_bounded_buffers_without_retaining_captures() {
    if !run_isolated("spare_pending_pool_reuses_empty_bounded_buffers_without_retaining_captures") {
        return;
    }
    let _test = TEST_LOCK.lock();
    while SPARE_PENDING.pop().is_some() {}

    let buffer = Vec::with_capacity(16);
    let allocation = buffer.as_ptr();
    recycle_pending_buffer(buffer);
    let reused = take_pending_buffer();
    assert_eq!(reused.as_ptr(), allocation);
    assert!(reused.is_empty());

    let drops = Arc::new(AtomicUsize::new(0));
    let capture = DropCapture(Arc::clone(&drops));
    let mut captured = reused;
    captured.push(PendingOp {
        id: Uuid::from_u128(32),
        height: 0,
        seq: 0,
        coalesce: false,
        run: Box::new(move || drop(capture)),
    });
    recycle_pending_buffer(captured);
    assert_eq!(drops.load(AtomicOrdering::Relaxed), 1);
    assert!(SPARE_PENDING.pop().is_some_and(|buffer| buffer.is_empty()));

    recycle_pending_buffer(Vec::with_capacity(
        MAX_SPARE_PENDING_CAPACITY.saturating_add(1),
    ));
    assert!(SPARE_PENDING.is_empty());

    for _ in 0..spare_pending_limit() {
        recycle_pending_buffer(Vec::with_capacity(1));
    }
    recycle_pending_buffer(Vec::with_capacity(1));
    assert_eq!(SPARE_PENDING.len(), spare_pending_limit());
    while let Some(buffer) = SPARE_PENDING.pop() {
        assert!(buffer.is_empty());
        assert!(buffer.capacity() <= MAX_SPARE_PENDING_CAPACITY);
    }

    let retained_bytes = spare_pending_limit()
        .saturating_mul(MAX_SPARE_PENDING_CAPACITY)
        .saturating_mul(std::mem::size_of::<PendingOp>());
    assert!(retained_bytes > 0);
    assert!(spare_pending_limit() <= MAX_SPARE_PENDING);
}

#[test]
fn superseded_callback_destructors_reenter_only_after_tick_unlocks() {
    if !run_isolated("superseded_callback_destructors_reenter_only_after_tick_unlocks") {
        return;
    }
    let _test = TEST_LOCK.lock();
    reset_global_tick(1, true);
    let drops = Arc::new(AtomicUsize::new(0));
    let (done_tx, done_rx) = sync_channel(0);
    let worker_drops = Arc::clone(&drops);
    let worker = std::thread::spawn(move || {
        let id = Uuid::from_u128(33);
        let first = ReenterSchedulerOnDrop(Arc::clone(&worker_drops));
        let discarded = {
            let mut tick = TICK.lock();
            let discarded = tick.enqueue_locked(id, 0, 10, true, Box::new(move || drop(first)));
            drop(tick);
            discarded
        };
        drop(discarded);

        let stale = ReenterSchedulerOnDrop(Arc::clone(&worker_drops));
        let discarded = {
            let mut tick = TICK.lock();
            let discarded = tick.enqueue_locked(id, 0, 9, true, Box::new(move || drop(stale)));
            drop(tick);
            discarded
        };
        drop(discarded);

        let collision = ReenterSchedulerOnDrop(Arc::clone(&worker_drops));
        let discarded = {
            let mut tick = TICK.lock();
            let discarded = tick.enqueue_locked(id, 0, 11, true, Box::new(move || drop(collision)));
            drop(tick);
            discarded
        };
        drop(discarded);

        let discarded = {
            let mut tick = TICK.lock();
            let discarded = tick.enqueue_locked(id, 0, 11, false, Box::new(|| {}));
            drop(tick);
            discarded
        };
        drop(discarded);
        let _ = done_tx.send(());
    });

    assert!(done_rx.recv_timeout(Duration::from_secs(2)).is_ok());
    assert!(worker.join().is_ok());
    assert_eq!(drops.load(AtomicOrdering::Relaxed), 3);
    reset_global_tick(0, false);
}

#[test]
fn buffer_recycle_limits_never_drop_pending_event_work() {
    if !run_isolated("buffer_recycle_limits_never_drop_pending_event_work") {
        return;
    }
    let _test = TEST_LOCK.lock();
    reset_global_tick(1, true);
    while SPARE_PENDING.pop().is_some() {}
    let completed = Arc::new(AtomicUsize::new(0));
    let id = Uuid::from_u128(34);
    let mut oversized = Vec::with_capacity(MAX_SPARE_PENDING_CAPACITY.saturating_add(1));
    for seq in 0u64..4097 {
        let completed = Arc::clone(&completed);
        oversized.push(PendingOp {
            id,
            height: 0,
            seq,
            coalesce: false,
            run: Box::new(move || {
                completed.fetch_add(1, AtomicOrdering::Relaxed);
            }),
        });
    }

    let phase = WaveSequencePhase::begin();
    let panics = merge_wave(
        vec![GroupResult {
            pending: oversized,
            panics: Vec::new(),
        }],
        phase,
    );
    assert!(panics.is_empty());
    assert!(SPARE_PENDING.is_empty());
    drain_global_order();
    assert_eq!(completed.load(AtomicOrdering::Relaxed), 4097);

    for _ in 0..spare_pending_limit() {
        recycle_pending_buffer(Vec::with_capacity(1));
    }
    let mut pending = Vec::with_capacity(3);
    for seq in 5000u64..5003 {
        let completed = Arc::clone(&completed);
        pending.push(PendingOp {
            id,
            height: 0,
            seq,
            coalesce: false,
            run: Box::new(move || {
                completed.fetch_add(1, AtomicOrdering::Relaxed);
            }),
        });
    }
    let phase = WaveSequencePhase::begin();
    let panics = merge_wave(
        vec![GroupResult {
            pending,
            panics: Vec::new(),
        }],
        phase,
    );
    assert!(panics.is_empty());
    assert_eq!(SPARE_PENDING.len(), spare_pending_limit());
    drain_global_order();
    assert_eq!(completed.load(AtomicOrdering::Relaxed), 4100);
    while SPARE_PENDING.pop().is_some() {}
    reset_global_tick(0, false);
}

#[test]
fn nested_group_scope_restores_the_outer_buffer() {
    let _test = TEST_LOCK.lock();
    GROUP_PENDING.with(|pending| pending.replace(None));
    let outer = PendingScope::enter();
    let node = Arc::new(TestNode::source(7));

    let inner = run_group_buffered(vec![{
        let node = Arc::clone(&node);
        deferred(move || enqueue(node.id, node.as_ref(), false, Box::new(|| {})))
    }]);
    enqueue(node.id, node.as_ref(), false, Box::new(|| {}));
    let outer_pending = outer.finish();

    assert_eq!(inner.pending.len(), 1);
    assert_eq!(outer_pending.len(), 1);
    assert!(GROUP_PENDING.with(|pending| pending.borrow().is_none()));
}

#[test]
fn batch_propagates_group_panic_after_pending_work_and_recovers() {
    if !run_isolated("batch_propagates_group_panic_after_pending_work_and_recovers") {
        return;
    }
    let _test = TEST_LOCK.lock();
    reset_global_tick(0, false);
    let previous_threshold = wave_threshold();
    WAVE_THRESHOLD.store(1, Ordering::Relaxed);
    let root = Arc::new(TestNode::source(23));
    let child = Arc::new(TestNode::source(24));
    let seen = Arc::new(Mutex::new(Vec::new()));

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
        let root = Arc::clone(&root);
        let child = Arc::clone(&child);
        let seen = Arc::clone(&seen);
        move || {
            batch(|| {
                enqueue(
                    root.id,
                    root.as_ref(),
                    false,
                    Box::new(move || {
                        enqueue(child.id, child.as_ref(), false, tagged_run(1, seen));
                        std::panic::resume_unwind(Box::new("group panic"));
                    }),
                );
            });
        }
    }));

    assert!(result.is_err());
    assert_eq!(*seen.lock(), vec![1]);
    let later_seen = Arc::clone(&seen);
    batch(|| {
        enqueue(root.id, root.as_ref(), false, tagged_run(2, later_seen));
    });
    assert_eq!(*seen.lock(), vec![1, 2]);
    assert!(GROUP_PENDING.with(|pending| pending.borrow().is_none()));
    WAVE_THRESHOLD.store(previous_threshold, Ordering::Relaxed);
    reset_global_tick(0, false);
}

#[test]
fn discarded_callback_drop_panic_settles_wave_then_recovers() {
    if !run_isolated("discarded_callback_drop_panic_settles_wave_then_recovers") {
        return;
    }
    let _test = TEST_LOCK.lock();
    reset_global_tick(0, false);
    let previous_threshold = wave_threshold();
    WAVE_THRESHOLD.store(1, Ordering::Relaxed);
    let root = Arc::new(TestNode::source(35));
    let target = Arc::new(TestNode::source(36));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    let (local_ready_tx, local_ready_rx) = sync_channel(0);
    let (external_done_tx, external_done_rx) = sync_channel(0);
    let external_target = Arc::clone(&target);
    let external_seen = Arc::clone(&seen);
    let external = std::thread::spawn(move || {
        if local_ready_rx.recv().is_ok() {
            enqueue(
                external_target.id,
                external_target.as_ref(),
                false,
                tagged_run(2, external_seen),
            );
            let _ = external_done_tx.send(());
        }
    });

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
        let root = Arc::clone(&root);
        let target = Arc::clone(&target);
        let drops = Arc::clone(&drops);
        move || {
            batch(|| {
                enqueue(
                    root.id,
                    root.as_ref(),
                    false,
                    Box::new(move || {
                        let capture = PanicOnDrop(drops);
                        enqueue(
                            target.id,
                            target.as_ref(),
                            false,
                            Box::new(move || drop(capture)),
                        );
                        let _ = local_ready_tx.send(());
                        let _ = external_done_rx.recv();
                    }),
                );
            });
        }
    }));

    assert!(result.is_err());
    assert!(external.join().is_ok());
    assert_eq!(drops.load(AtomicOrdering::Relaxed), 1);
    assert_eq!(*seen.lock(), vec![2]);
    {
        let tick = TICK.lock();
        assert!(!tick.draining);
        assert!(tick.order.is_empty());
        drop(tick);
    }

    let later_seen = Arc::clone(&seen);
    batch(|| {
        enqueue(root.id, root.as_ref(), false, tagged_run(3, later_seen));
    });
    assert_eq!(*seen.lock(), vec![2, 3]);
    WAVE_THRESHOLD.store(previous_threshold, Ordering::Relaxed);
    reset_global_tick(0, false);
}

fn handoff_op(remaining: usize, node: Arc<TestNode>, completed: Arc<AtomicUsize>) -> DeferredOp {
    Box::new(move || {
        completed.fetch_add(1, AtomicOrdering::Relaxed);
        if remaining > 1 {
            let next_node = Arc::clone(&node);
            enqueue(
                node.id,
                node.as_ref(),
                true,
                handoff_op(remaining.saturating_sub(1), next_node, completed),
            );
        }
    })
}

#[test]
fn sixty_four_wave_handoff_leaves_buffered_successor_visible() {
    if !run_isolated("sixty_four_wave_handoff_leaves_buffered_successor_visible") {
        return;
    }
    let _test = TEST_LOCK.lock();
    reset_global_tick(0, false);
    let previous_threshold = wave_threshold();
    WAVE_THRESHOLD.store(1, Ordering::Relaxed);
    let node = Arc::new(TestNode::source(25));
    let completed = Arc::new(AtomicUsize::new(0));
    let (ready_tx, ready_rx) = sync_channel(0);
    let (release_tx, release_rx) = sync_channel(0);
    let peer = std::thread::spawn(move || {
        batch(|| {
            assert!(ready_tx.send(()).is_ok());
            assert!(release_rx.recv().is_ok());
        });
    });
    assert!(ready_rx.recv().is_ok());
    batch(|| {
        enqueue(
            node.id,
            node.as_ref(),
            true,
            handoff_op(65, Arc::clone(&node), Arc::clone(&completed)),
        );
    });

    assert_eq!(completed.load(AtomicOrdering::Relaxed), 64);
    {
        let tick = TICK.lock();
        assert_eq!(tick.order.len(), 1);
        assert!(!tick.draining);
        assert_eq!(tick.depth, 1);
        drop(tick);
    }
    assert!(release_tx.send(()).is_ok());
    assert!(peer.join().is_ok());
    assert_eq!(completed.load(AtomicOrdering::Relaxed), 65);
    WAVE_THRESHOLD.store(previous_threshold, Ordering::Relaxed);
    reset_global_tick(0, false);
}

#[test]
fn wide_parallel_wave_uses_no_worker_enqueue_locks_and_one_merge() {
    if !run_isolated("wide_parallel_wave_uses_no_worker_enqueue_locks_and_one_merge") {
        return;
    }
    let _test = TEST_LOCK.lock();
    let previous_threshold = wave_threshold();
    WAVE_THRESHOLD.store(1, Ordering::Relaxed);
    ENQUEUE_LOCK_ACQUISITIONS.store(0, Ordering::Relaxed);
    WAVE_MERGE_ACQUISITIONS.store(0, Ordering::Relaxed);
    {
        let mut tick = TICK.lock();
        tick.order.clear();
        tick.scheduled.clear();
        tick.depth = 1;
        tick.draining = true;
        drop(tick);
    }

    let groups = (0u128..256)
        .map(|group| {
            let node = Arc::new(TestNode::source(group.saturating_add(100)));
            vec![deferred(move || {
                for notification in 0u128..16 {
                    let id = Uuid::from_u128(
                        group
                            .saturating_mul(16)
                            .saturating_add(notification)
                            .saturating_add(10_000),
                    );
                    enqueue(id, node.as_ref(), false, Box::new(|| {}));
                }
            })]
        })
        .collect();

    let panics = run_wave(groups);

    assert!(panics.is_empty());
    assert_eq!(ENQUEUE_LOCK_ACQUISITIONS.load(Ordering::Relaxed), 0);
    assert_eq!(WAVE_MERGE_ACQUISITIONS.load(Ordering::Relaxed), 1);
    assert_eq!(TICK.lock().order.len(), 4096);
    {
        let mut tick = TICK.lock();
        tick.order.clear();
        tick.scheduled.clear();
        tick.depth = 0;
        tick.draining = false;
        refresh_tick_active(&tick);
        drop(tick);
    }
    WAVE_THRESHOLD.store(previous_threshold, Ordering::Relaxed);
}
