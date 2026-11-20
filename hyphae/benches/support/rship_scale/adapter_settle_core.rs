//! hyphae-settle-core: one settle lock, runs of event sets, wave-parallel
//! firing. myko keys a client write `Exclusive` so two values of one entity
//! never share a settle; the suite does the same per writer.

use hyphae::{Cell, CellImmutable, CellMap, CellMutable, Materialize, SettleKey, SwitchMapExt};

pub const NAME: &str = "core";

pub fn init(workers: usize) {
    // SAFETY: called first in `main`, before any thread exists.
    unsafe { std::env::set_var("HYPHAE_WORKER_THREADS", workers.to_string()) };
    hyphae::settle::set_workers(workers);
}

pub fn write<R>(key: u64, f: impl FnOnce() -> R) -> R {
    hyphae::batch_with(&[SettleKey::Exclusive(key)], None, f)
}

pub fn post(key: u64, f: impl FnOnce()) {
    hyphae::submit(&[SettleKey::Exclusive(key)], None, f);
}

pub fn flush() {
    hyphae::flush();
}

pub fn from_callback<R>(f: impl FnOnce() -> R) -> R {
    f()
}

/// Build an event-semantic subgraph. Nothing to do: the writer's
/// `Exclusive` key keeps two of its writes out of one settle.
pub fn events<R>(f: impl FnOnce() -> R) -> R {
    f()
}

pub fn counters() -> Vec<(String, u64)> {
    super::debug_counters(&format!("{:?}", hyphae::settle::stats()))
}

#[derive(Clone)]
pub struct Table {
    map: CellMap<u32, u64, CellMutable>,
}

impl Table {
    pub fn new(map: &CellMap<u32, u64, CellMutable>) -> Self {
        Self { map: map.clone() }
    }

    pub fn lookup(&self, key: &Cell<u32, CellMutable>) -> Cell<Option<u64>, CellImmutable> {
        let map = self.map.clone();
        key.clone()
            .switch_map(move |k| map.get(k).materialize())
            .materialize()
    }
}
