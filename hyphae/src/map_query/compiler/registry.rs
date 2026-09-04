use std::{
    any::{Any, TypeId},
    collections::{HashMap, hash_map::Entry},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use super::SourceIdentity;

pub(super) struct RootRequirement {
    pub(super) ordinal: usize,
    pub(super) uses: usize,
    pub(super) typed_sinks: Box<dyn Any>,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) struct RootKey {
    pub(super) source: SourceIdentity,
    pub(super) key: TypeId,
    pub(super) value: TypeId,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) struct RelationshipKey {
    pub(super) source: SourceIdentity,
    pub(super) relation: TypeId,
}

pub(super) trait PhysicalRelationshipBinding {
    fn bind(&self, key: RelationshipKey, indexes: &mut HashMap<RelationshipKey, Box<dyn Any>>);
}

pub(super) struct TypedRelationshipBinding<T> {
    pub(super) slot: DeferredPhysical<T>,
}

impl<T> PhysicalRelationshipBinding for TypedRelationshipBinding<T>
where
    T: Default + Send + Sync + 'static,
{
    fn bind(&self, key: RelationshipKey, indexes: &mut HashMap<RelationshipKey, Box<dyn Any>>) {
        match indexes.entry(key) {
            Entry::Occupied(entry) => {
                let index = entry.get().downcast_ref::<Arc<parking_lot::RwLock<T>>>();
                assert!(
                    index.is_some(),
                    "compiler invariant violated: relationship index type mismatch"
                );
                let Some(index) = index else {
                    return;
                };
                let binding = self.slot.inner.set(Arc::clone(index));
                assert!(
                    binding.is_ok(),
                    "compiler invariant violated: physical index slot already bound"
                );
                self.slot.maintains_index.store(false, Ordering::Release);
            }
            Entry::Vacant(entry) => {
                let index = Arc::new(parking_lot::RwLock::new(T::default()));
                let binding = self.slot.inner.set(Arc::clone(&index));
                assert!(
                    binding.is_ok(),
                    "compiler invariant violated: physical index slot already bound"
                );
                entry.insert(Box::new(index));
                self.slot.maintains_index.store(true, Ordering::Release);
            }
        }
    }
}

/// A typed direct handle populated when the owning right subtree resolves to
/// its physical source during compilation.
pub struct DeferredPhysical<T> {
    inner: Arc<OnceLock<Arc<parking_lot::RwLock<T>>>>,
    maintains_index: Arc<AtomicBool>,
}

impl<T> Clone for DeferredPhysical<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            maintains_index: Arc::clone(&self.maintains_index),
        }
    }
}

impl<T> Default for DeferredPhysical<T> {
    fn default() -> Self {
        Self {
            inner: Arc::new(OnceLock::new()),
            maintains_index: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl<T> DeferredPhysical<T>
where
    T: Default,
{
    pub(crate) fn acquire_read(&self) -> parking_lot::RwLockReadGuard<'_, T> {
        self.inner
            .get_or_init(|| Arc::new(parking_lot::RwLock::new(T::default())))
            .read()
    }

    pub(crate) fn write<R>(&self, write: impl FnOnce(&mut T) -> R) -> R {
        let index = self
            .inner
            .get_or_init(|| Arc::new(parking_lot::RwLock::new(T::default())));
        let mut index = index.write();
        write(&mut index)
    }

    pub(crate) fn maintains_index(&self) -> bool {
        self.maintains_index.load(Ordering::Acquire)
    }
}
