//! Synchronous write hooks: callbacks that run inside every mutation of a
//! map's data, under the shard lock that guards the written key.

use std::{
    hash::Hash,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use arc_swap::ArcSwap;
use dashmap::mapref::entry::Entry;

use super::{CellMap, CellMapInner, SHARDS};
use crate::{subscription::SubscriptionGuard, traits::CellValue};

/// One change to one key, as seen by a write hook.
///
/// `Update` is only reported when the new value differs from the old one:
/// writes the map suppresses as equal never reach a hook.
#[derive(Debug, PartialEq, Eq)]
pub enum MapWrite<'a, K, V> {
    /// `key` was absent and now holds `value`.
    Insert { key: &'a K, value: &'a V },
    /// `key` held `old` and now holds `new`.
    Update { key: &'a K, old: &'a V, new: &'a V },
    /// `key` held `old` and is now absent.
    Remove { key: &'a K, old: &'a V },
}

impl<K, V> Clone for MapWrite<'_, K, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K, V> Copy for MapWrite<'_, K, V> {}

/// Outcome of [`CellMap::store`].
pub(super) enum Stored<V> {
    Inserted,
    /// `equal` is known when a hook needed it under the lock.
    Replaced {
        old: V,
        equal: Option<bool>,
    },
}

impl<V: PartialEq> Stored<V> {
    /// Whether the write left an equal value in place.
    pub(super) fn unchanged(&self, new: &V) -> bool {
        match self {
            Self::Inserted => false,
            Self::Replaced { old, equal } => equal.unwrap_or_else(|| old == new),
        }
    }

    pub(super) fn into_old(self) -> Option<V> {
        match self {
            Self::Inserted => None,
            Self::Replaced { old, .. } => Some(old),
        }
    }
}

type Hook<K, V> = Arc<dyn Fn(MapWrite<'_, K, V>) + Send + Sync>;

pub(super) struct WriteHooks<K, V> {
    /// Read under a shard lock, written under every shard lock, so `Relaxed`
    /// is ordered by the locks themselves.
    active: AtomicBool,
    hooks: ArcSwap<Vec<(u64, Hook<K, V>)>>,
    next_id: AtomicU64,
    /// Per-shard count of hooked writes, bumped under the shard write lock.
    /// An equality check made under a read lock is still valid under the
    /// write lock when its shard's count has not moved.
    writes: [AtomicU64; SHARDS],
}

impl<K, V> WriteHooks<K, V> {
    pub(super) fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            hooks: ArcSwap::from_pointee(Vec::new()),
            next_id: AtomicU64::new(0),
            writes: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    #[inline]
    fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Count a write to `shard`. Called under its write lock.
    fn wrote(&self, shard: usize) {
        if let Some(writes) = self.writes.get(shard) {
            writes.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn writes(&self, shard: usize) -> Option<u64> {
        self.writes
            .get(shard)
            .map(|writes| writes.load(Ordering::Relaxed))
    }

    fn fire(&self, write: MapWrite<'_, K, V>) {
        for (_, hook) in self.hooks.load().iter() {
            hook(write);
        }
    }
}

impl<K, V, M> CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Register a hook that runs synchronously inside every change to this
    /// map's data. See the [module docs](crate::cell_map#write-hooks) for
    /// the full contract.
    ///
    /// Registration first replays every current entry to `hook` as a
    /// [`MapWrite::Insert`], then installs it, with every shard write-locked
    /// for the whole step: each concurrent write lands either in the replay
    /// or in a later hook call, never both and never neither.
    ///
    /// The hook runs under the shard lock of the key it reports. It must not
    /// read or write this map, register or drop a write hook on it, or block
    /// for long. It sees borrowed keys and values and is called once per
    /// changed key, in the order the map applies writes to that key.
    ///
    /// Dropping the returned guard removes the hook.
    pub fn register_write_hook<F>(&self, hook: F) -> SubscriptionGuard
    where
        F: Fn(MapWrite<'_, K, V>) + Send + Sync + 'static,
    {
        let hook: Hook<K, V> = Arc::new(hook);
        let hooks = &self.inner.write_hooks;
        let id = hooks.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let tables: Vec<_> = self.inner.data.shards().iter().map(|s| s.write()).collect();
            for table in &tables {
                // SAFETY: the write guard outlives the iterator and every
                // bucket reference, and no other thread can touch the table.
                for bucket in unsafe { table.iter() } {
                    let (key, value) = unsafe { bucket.as_ref() };
                    hook(MapWrite::Insert {
                        key,
                        value: value.get(),
                    });
                }
            }
            let mut next = Vec::clone(&hooks.hooks.load());
            next.push((id, hook));
            hooks.hooks.store(Arc::new(next));
            hooks.active.store(true, Ordering::Relaxed);
            (0..SHARDS).for_each(|shard| hooks.wrote(shard));
        }
        let weak = Arc::downgrade(&self.inner);
        let mut weak = Some(weak);
        SubscriptionGuard::from_callback(move || {
            if let Some(inner) = weak.take().and_then(|w| w.upgrade()) {
                remove_hook(&inner, id);
            }
        })
    }

    /// Store `value` under `key`, reporting the change to write hooks while
    /// the shard lock is held. The caller drops the previous value after the
    /// lock is released.
    pub(super) fn store(&self, key: K, value: V) -> Stored<V> {
        let hooks = &self.inner.write_hooks;
        let data = &self.inner.data;
        // With a hook, compare under a read lock first, so a deep `PartialEq`
        // blocks other writers of the shard but not its readers. The write
        // lock reuses the answer unless the shard was written in between.
        let checked = hooks.active().then(|| {
            let shard = data.determine_map(&key);
            let writes = hooks.writes(shard);
            let equal = data.get(&key).map(|current| current.value() == &value);
            (shard, writes, equal)
        });
        match data.entry(key) {
            Entry::Occupied(mut entry) => {
                let old = entry.insert(value);
                // Without a hook, callers that suppress equal writes compare
                // after the lock is released, as they always have.
                let equal = hooks.active().then(|| {
                    let shard = data.determine_map(entry.key());
                    let reused = checked.and_then(|(checked_shard, writes, equal)| {
                        (checked_shard == shard && hooks.writes(shard) == writes)
                            .then_some(equal)
                            .flatten()
                    });
                    hooks.wrote(shard);
                    reused.unwrap_or_else(|| &old == entry.get())
                });
                if equal == Some(false) {
                    hooks.fire(MapWrite::Update {
                        key: entry.key(),
                        old: &old,
                        new: entry.get(),
                    });
                }
                drop(entry);
                Stored::Replaced { old, equal }
            }
            Entry::Vacant(entry) => {
                let stored = entry.insert(value);
                if hooks.active() {
                    hooks.wrote(data.determine_map(stored.key()));
                    hooks.fire(MapWrite::Insert {
                        key: stored.key(),
                        value: stored.value(),
                    });
                }
                Stored::Inserted
            }
        }
    }

    /// Remove `key`, reporting the removal to write hooks while the shard
    /// lock is held. The caller drops the returned pair after the lock is
    /// released.
    pub(super) fn delete(&self, key: &K) -> Option<(K, V)> {
        let hooks = &self.inner.write_hooks;
        self.inner.data.remove_if(key, |key, old| {
            if hooks.active() {
                hooks.wrote(self.inner.data.determine_map(key));
                hooks.fire(MapWrite::Remove { key, old });
            }
            true
        })
    }
}

fn remove_hook<K, V>(inner: &CellMapInner<K, V>, id: u64)
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    let hooks = &inner.write_hooks;
    let tables: Vec<_> = inner.data.shards().iter().map(|s| s.write()).collect();
    let mut next = Vec::clone(&hooks.hooks.load());
    next.retain(|(hook_id, _)| *hook_id != id);
    hooks.active.store(!next.is_empty(), Ordering::Relaxed);
    let previous = hooks.hooks.swap(Arc::new(next));
    drop(tables);
    // The removed hook's captures drop here, outside every shard lock.
    drop(previous);
}
