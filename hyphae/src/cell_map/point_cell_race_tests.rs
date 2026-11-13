use std::sync::mpsc;

use super::*;
use crate::traits::Gettable;

enum Progress {
    CandidateClone,
    InitialComparison,
    Ready(Cell<Option<ProbedValue>, CellImmutable>),
}

#[derive(Debug)]
struct Gates {
    armed: AtomicBool,
    clones: AtomicUsize,
    compared: AtomicBool,
    progress: mpsc::Sender<Progress>,
    resume: Mutex<mpsc::Receiver<()>>,
}

#[derive(Debug)]
struct ProbedValue {
    value: u8,
    gates: Arc<Gates>,
}

impl Clone for ProbedValue {
    fn clone(&self) -> Self {
        if self.value == 0
            && self.gates.armed.load(Ordering::Acquire)
            && self.gates.clones.fetch_add(1, Ordering::Relaxed) == 1
        {
            assert!(self.gates.progress.send(Progress::CandidateClone).is_ok());
            assert!(self.gates.resume.lock().recv().is_ok());
        }
        Self {
            value: self.value,
            gates: self.gates.clone(),
        }
    }
}

impl PartialEq for ProbedValue {
    fn eq(&self, other: &Self) -> bool {
        if self.value == 0
            && other.value == 0
            && self.gates.armed.load(Ordering::Acquire)
            && !self.gates.compared.swap(true, Ordering::AcqRel)
        {
            assert!(
                self.gates
                    .progress
                    .send(Progress::InitialComparison)
                    .is_ok()
            );
            assert!(self.gates.resume.lock().recv().is_ok());
        }
        self.value == other.value
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CollidingKey(u8);

impl Hash for CollidingKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        0_u8.hash(state);
    }
}

struct ReentrantValue {
    value: u8,
    read: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Clone for ReentrantValue {
    fn clone(&self) -> Self {
        if let Some(read) = &self.read {
            read();
        }
        Self {
            value: self.value,
            read: self.read.clone(),
        }
    }
}

impl std::fmt::Debug for ReentrantValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.value, formatter)
    }
}

impl PartialEq for ReentrantValue {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

#[test]
fn point_value_clone_can_read_another_point_with_a_colliding_key() {
    let source = CellMap::<CollidingKey, ReentrantValue>::new();
    let armed = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let read_source = source.downgrade();
    let read_armed = armed.clone();
    let read_count = reads.clone();
    let read = Arc::new(move || {
        if read_armed.load(Ordering::Acquire) {
            let map = read_source.upgrade();
            assert!(map.is_some());
            if let Some(map) = map {
                let other = map.get(&CollidingKey(2)).materialize();
                assert_eq!(other.get().map(|value| value.value), Some(2));
                read_count.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
    source.insert(
        CollidingKey(1),
        ReentrantValue {
            value: 1,
            read: Some(read),
        },
    );
    source.insert(
        CollidingKey(2),
        ReentrantValue {
            value: 2,
            read: None,
        },
    );
    armed.store(true, Ordering::Release);
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let point = source.get(&CollidingKey(1)).materialize();
        assert!(
            finished_tx
                .send(point.get().map(|value| value.value))
                .is_ok()
        );
    });
    let finished = finished_rx.recv_timeout(std::time::Duration::from_secs(5));
    assert_eq!(finished, Ok(Some(1)));
    assert!(worker.join().is_ok());
    assert!(reads.load(Ordering::Relaxed) > 0);
}

#[derive(Clone)]
struct ReentrantKey {
    value: u8,
    hash_read: Option<Arc<dyn Fn() + Send + Sync>>,
    debug_read: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for ReentrantKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(read) = &self.debug_read {
            read();
        }
        std::fmt::Debug::fmt(&self.value, formatter)
    }
}

impl PartialEq for ReentrantKey {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for ReentrantKey {}

impl Hash for ReentrantKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        if let Some(read) = &self.hash_read {
            read();
        }
        0_u8.hash(state);
    }
}

struct PublicationGate {
    armed: AtomicBool,
    paused: mpsc::Sender<()>,
    resume: Mutex<mpsc::Receiver<()>>,
}

#[derive(Clone)]
struct PublicationKey {
    value: u8,
    gate: Arc<PublicationGate>,
}

impl std::fmt::Debug for PublicationKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.gate.armed.swap(false, Ordering::AcqRel) {
            self.gate.paused.send(()).map_err(|_| std::fmt::Error)?;
            self.gate
                .resume
                .lock()
                .recv()
                .map_err(|_| std::fmt::Error)?;
        }
        std::fmt::Debug::fmt(&self.value, formatter)
    }
}

impl PartialEq for PublicationKey {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for PublicationKey {}

impl Hash for PublicationKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

fn point_seed_tracks_write(
    mutate: impl FnOnce(&CellMap<PublicationKey, u8>, PublicationKey),
    expected: Option<u8>,
) {
    let (paused_tx, paused_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let key = PublicationKey {
        value: 7,
        gate: Arc::new(PublicationGate {
            armed: AtomicBool::new(false),
            paused: paused_tx,
            resume: Mutex::new(resume_rx),
        }),
    };
    let source = CellMap::new().with_name("point-publication-race");
    source.insert(key.clone(), 0);
    key.gate.armed.store(true, Ordering::Release);
    let get_source = source.clone();
    let get_key = key.clone();
    let getter = std::thread::spawn(move || get_source.get(&get_key).materialize());
    assert!(
        paused_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_ok()
    );
    mutate(&source, key.clone());
    assert!(resume_tx.send(()).is_ok());
    let point = getter.join();
    assert!(point.is_ok());
    let Ok(point) = point else { return };
    assert_eq!(source.get_value(&key), expected);
    assert_eq!(point.get(), expected);
}

#[test]
fn point_seed_retries_writes_before_cache_publication() {
    point_seed_tracks_write(
        |map, key| {
            map.insert(key, 1);
        },
        Some(1),
    );
    point_seed_tracks_write(|map, key| map.insert_many(vec![(key, 1)]), Some(1));
    point_seed_tracks_write(
        |map, key| {
            map.remove(&key);
        },
        None,
    );
    point_seed_tracks_write(|map, key| map.remove_many(vec![key]), None);
    point_seed_tracks_write(|map, key| map.replace_all(vec![(key, 1)]), Some(1));
    point_seed_tracks_write(|map, _| map.replace_all(Vec::new()), None);
    point_seed_tracks_write(
        |map, key| {
            map.apply_diff_owned(MapDiff::Initial {
                entries: vec![(key, 1)],
            });
        },
        Some(1),
    );
    point_seed_tracks_write(
        |map, _| {
            map.apply_diff_owned(MapDiff::Initial {
                entries: Vec::new(),
            });
        },
        None,
    );
    point_seed_tracks_write(
        |map, key| {
            map.apply_batch(vec![MapDiff::Initial {
                entries: vec![(key, 1)],
            }]);
        },
        Some(1),
    );
    point_seed_tracks_write(
        |map, _| {
            map.apply_batch(vec![MapDiff::Initial {
                entries: Vec::new(),
            }]);
        },
        None,
    );
}

fn reentrant_key_reads_other_point(from_debug: bool) {
    let source = CellMap::<ReentrantKey, u8>::new();
    let source = if from_debug {
        source.with_name("reentrant-key-debug")
    } else {
        source
    };
    source.insert(
        ReentrantKey {
            value: 2,
            hash_read: None,
            debug_read: None,
        },
        2,
    );
    let read_source = source.downgrade();
    let reads = Arc::new(AtomicUsize::new(0));
    let read_count = reads.clone();
    let read: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let map = read_source.upgrade();
        assert!(map.is_some());
        if let Some(map) = map {
            let other = map
                .get(&ReentrantKey {
                    value: 2,
                    hash_read: None,
                    debug_read: None,
                })
                .materialize();
            assert_eq!(other.get(), Some(2));
            read_count.fetch_add(1, Ordering::Relaxed);
        }
    });
    let key = ReentrantKey {
        value: 1,
        hash_read: (!from_debug).then(|| read.clone()),
        debug_read: from_debug.then_some(read),
    };
    let get_source = source.clone();
    let get_key = key.clone();
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let point = get_source.get(&get_key).materialize();
        assert!(finished_tx.send(point).is_ok());
    });
    let finished = finished_rx.recv_timeout(std::time::Duration::from_secs(5));
    assert!(finished.is_ok());
    let Ok(point) = finished else { return };
    assert!(worker.join().is_ok());
    assert_eq!(point.get(), None);
    source.insert(key, 9);
    assert_eq!(point.get(), Some(9));
    assert!(reads.load(Ordering::Relaxed) > 0);
}

#[test]
fn missing_point_key_hash_can_read_another_ephemeral_point() {
    reentrant_key_reads_other_point(false);
}

#[test]
fn named_point_key_debug_can_read_another_named_point() {
    reentrant_key_reads_other_point(true);
}

#[test]
fn point_name_debug_can_mutate_its_own_key_without_repeated_initialization() {
    let source = CellMap::<ReentrantKey, u8>::new().with_name("point-name-write");
    let weak = source.downgrade();
    let enabled = Arc::new(AtomicBool::new(true));
    let alternate = Arc::new(AtomicBool::new(false));
    let active = enabled.clone();
    let key = ReentrantKey {
        value: 1,
        hash_read: None,
        debug_read: Some(Arc::new(move || {
            if active.load(Ordering::Acquire)
                && let Some(map) = weak.upgrade()
            {
                let value = if alternate.fetch_xor(true, Ordering::Relaxed) {
                    88
                } else {
                    77
                };
                map.insert(
                    ReentrantKey {
                        value: 1,
                        hash_read: None,
                        debug_read: None,
                    },
                    value,
                );
            }
        })),
    };
    let map = source.clone();
    let requested = key.clone();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        assert!(tx.send(map.get(&requested).materialize()).is_ok());
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(5));
    let completed = result.is_ok();
    enabled.store(false, Ordering::Release);
    let result = result.or_else(|_| rx.recv_timeout(std::time::Duration::from_secs(5)));
    assert!(worker.join().is_ok());
    assert!(
        completed,
        "name formatting repeatedly invalidated the point seed"
    );
    assert!(result.is_ok());
    let Ok(point) = result else { return };
    assert_eq!(source.get_value(&key), Some(77));
    assert_eq!(point.get(), Some(77));
    source.insert(key, 99);
    assert_eq!(point.get(), Some(99));
}

fn key_on_another_data_shard(source: &CellMap<u64, ReentrantValue>) -> Option<u64> {
    for key in 1_u64..256 {
        source.insert(
            key,
            ReentrantValue {
                value: 2,
                read: None,
            },
        );
        let held = source.inner.data.get(&0);
        assert!(held.is_some());
        let independent = matches!(
            source.inner.data.try_get_mut(&key),
            dashmap::try_result::TryResult::Present(_)
        );
        drop(held);
        if independent {
            return Some(key);
        }
        source.remove(&key);
    }
    None
}

#[test]
fn reserved_point_slot_survives_pruning_and_shares_a_concurrent_winner() {
    let source = CellMap::<u64, ReentrantValue>::new();
    source.insert(
        0,
        ReentrantValue {
            value: 1,
            read: None,
        },
    );
    let unrelated = key_on_another_data_shard(&source);
    assert!(unrelated.is_some());
    let Some(unrelated) = unrelated else { return };
    let armed = Arc::new(AtomicBool::new(false));
    let read_armed = armed.clone();
    let (paused_tx, paused_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let resume_rx = Mutex::new(resume_rx);
    let read = Arc::new(move || {
        if read_armed.swap(false, Ordering::AcqRel) {
            assert!(paused_tx.send(()).is_ok());
            assert!(resume_rx.lock().recv().is_ok());
        }
    });
    source.insert(
        0,
        ReentrantValue {
            value: 1,
            read: Some(read),
        },
    );
    armed.store(true, Ordering::Release);
    let get_source = source.clone();
    let getter = std::thread::spawn(move || get_source.get(&0).materialize());
    assert!(
        paused_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_ok()
    );
    for value in 0..64 {
        source.insert(unrelated, ReentrantValue { value, read: None });
    }
    let second = source.get(&0).materialize();
    assert!(resume_tx.send(()).is_ok());
    let first = getter.join();
    assert!(first.is_ok());
    let Ok(first) = first else { return };
    assert_eq!(first.get().map(|value| value.value), Some(1));
    assert_eq!(second.get().map(|value| value.value), Some(1));
    source.insert(
        0,
        ReentrantValue {
            value: 9,
            read: None,
        },
    );
    assert_eq!(first.get().map(|value| value.value), Some(9));
    assert_eq!(second.get().map(|value| value.value), Some(9));
}

#[test]
fn point_value_clone_can_mutate_an_unrelated_key_while_its_slot_is_reserved() {
    let source = CellMap::<u64, ReentrantValue>::new();
    source.insert(
        0,
        ReentrantValue {
            value: 1,
            read: None,
        },
    );
    let unrelated = key_on_another_data_shard(&source);
    assert!(unrelated.is_some());
    let Some(unrelated) = unrelated else { return };
    let active = Arc::new(AtomicBool::new(false));
    let writes = Arc::new(AtomicUsize::new(0));
    let write_source = source.downgrade();
    let write_active = active.clone();
    let write_count = writes.clone();
    let read = Arc::new(move || {
        if write_active.load(Ordering::Acquire) {
            let map = write_source.upgrade();
            assert!(map.is_some());
            if let Some(map) = map {
                for _ in 0..64 {
                    let next = map
                        .get_value(&unrelated)
                        .map_or(0, |value| value.value.wrapping_add(1));
                    map.insert(
                        unrelated,
                        ReentrantValue {
                            value: next,
                            read: None,
                        },
                    );
                    write_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    });
    source.insert(
        0,
        ReentrantValue {
            value: 1,
            read: Some(read),
        },
    );
    active.store(true, Ordering::Release);
    let get_source = source.clone();
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        assert!(finished_tx.send(get_source.get(&0).materialize()).is_ok());
    });
    let finished = finished_rx.recv_timeout(std::time::Duration::from_secs(5));
    active.store(false, Ordering::Release);
    let promptly_finished = finished.is_ok();
    let finished =
        finished.or_else(|_| finished_rx.recv_timeout(std::time::Duration::from_secs(5)));
    assert!(finished.is_ok());
    let Ok(point) = finished else { return };
    assert!(worker.join().is_ok());
    assert!(promptly_finished);
    assert_eq!(point.get().map(|value| value.value), Some(1));
    source.insert(
        0,
        ReentrantValue {
            value: 9,
            read: None,
        },
    );
    assert_eq!(point.get().map(|value| value.value), Some(9));
    assert!(writes.load(Ordering::Relaxed) >= 64);
}

#[test]
fn concurrent_reinsertion_cannot_be_overwritten_by_point_cell_initialization() {
    let (progress_tx, progress_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let gates = Arc::new(Gates {
        armed: AtomicBool::new(false),
        clones: AtomicUsize::new(0),
        compared: AtomicBool::new(false),
        progress: progress_tx.clone(),
        resume: Mutex::new(resume_rx),
    });
    let source = CellMap::new();
    source.insert(
        7,
        ProbedValue {
            value: 0,
            gates: gates.clone(),
        },
    );
    gates.armed.store(true, Ordering::Release);
    let get_source = source.clone();
    let getter = std::thread::spawn(move || {
        let point = get_source.get(&7).materialize();
        assert!(progress_tx.send(Progress::Ready(point)).is_ok());
    });
    let mut paused = false;
    let point = loop {
        let progress = progress_rx.recv_timeout(std::time::Duration::from_secs(5));
        assert!(progress.is_ok());
        let Ok(progress) = progress else { return };
        match progress {
            Progress::CandidateClone => {
                paused = true;
                source.remove(&7);
                assert!(resume_tx.send(()).is_ok());
            }
            Progress::InitialComparison => {
                source.insert(
                    7,
                    ProbedValue {
                        value: 1,
                        gates: gates.clone(),
                    },
                );
                assert!(resume_tx.send(()).is_ok());
            }
            Progress::Ready(point) => break point,
        }
    };
    assert!(getter.join().is_ok());
    gates.armed.store(false, Ordering::Release);
    if !paused {
        source.remove(&7);
        source.insert(7, ProbedValue { value: 1, gates });
    }
    assert_eq!(source.get_value(&7).map(|value| value.value), Some(1));
    assert_eq!(point.get().map(|value| value.value), Some(1));
}
