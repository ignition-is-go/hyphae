//! v3.1.12: eager push outside a batch, one process-wide deferred queue
//! inside. There is no non-waiting write and no flush.

use hyphae::{Cell, CellImmutable, CellMap, CellMutable, Materialize, SwitchMapExt};

pub const NAME: &str = "v3112";

pub fn init(workers: usize) {
    // SAFETY: called first in `main`, before any thread exists.
    unsafe { std::env::set_var("HYPHAE_WORKER_THREADS", workers.to_string()) };
}

pub fn write<R>(_key: u64, f: impl FnOnce() -> R) -> R {
    hyphae::batch(f)
}

pub fn post(_key: u64, f: impl FnOnce()) {
    hyphae::batch(f);
}

pub fn flush() {}

pub fn from_callback<R>(f: impl FnOnce() -> R) -> R {
    f()
}

/// Build an event-semantic subgraph: every write to it must reach its sinks.
/// v3.1.12 coalesces queued writes per cell unless the cell is born here,
/// which is how rship builds its event nodes.
pub fn events<R>(f: impl FnOnce() -> R) -> R {
    hyphae::scheduler::no_coalesce(f)
}

pub fn counters() -> Vec<(String, u64)> {
    Vec::new()
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
