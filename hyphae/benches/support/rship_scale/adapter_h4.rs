//! h4: pipelined tickets, no settle lock, no switch_map. A keyed lookup is a
//! family row; a callback write is its own ticket, as myko and rship mark it.

use hyphae::{
    Cell, CellImmutable, CellMap, CellMutable, KeyedFamily, LookupExt, Materialize, family,
};

pub const NAME: &str = "h4";

pub fn init(workers: usize) {
    // SAFETY: called first in `main`, before any thread exists.
    unsafe { std::env::set_var("HYPHAE_WORKER_THREADS", workers.to_string()) };
    hyphae::settle::set_workers(workers);
}

pub fn write<R>(_key: u64, f: impl FnOnce() -> R) -> R {
    hyphae::batch_with(&[], None, f)
}

pub fn post(_key: u64, f: impl FnOnce()) {
    hyphae::submit(&[], None, f);
}

pub fn flush() {
    hyphae::flush();
}

pub fn from_callback<R>(f: impl FnOnce() -> R) -> R {
    hyphae::settle::new_ticket(f)
}

/// Build an event-semantic subgraph. Nothing to do: every write is its own
/// ticket.
pub fn events<R>(f: impl FnOnce() -> R) -> R {
    f()
}

pub fn counters() -> Vec<(String, u64)> {
    super::debug_counters(&format!("{:?}", hyphae::settle::stats()))
}

#[derive(Clone)]
pub struct Table {
    rows: KeyedFamily<u32, Cell<Option<u64>, CellImmutable>>,
}

impl Table {
    pub fn new(map: &CellMap<u32, u64, CellMutable>) -> Self {
        let source = map.clone();
        let rows = family(move |k: &u32| source.get(k).materialize());
        rows.anchor(map);
        Self { rows }
    }

    pub fn lookup(&self, key: &Cell<u32, CellMutable>) -> Cell<Option<u64>, CellImmutable> {
        key.clone().lookup(self.rows.clone())
    }
}
