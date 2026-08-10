use std::{
    any::{Any, TypeId},
    collections::{HashMap, VecDeque},
    hash::Hash,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    cell_map::MapDiff,
    subscription::SubscriptionGuard,
    traits::{CellValue, reactive_map::ReactiveMap},
};

use super::{BoxedMapDiffSink, MapDiffSink};

/// Stable identity of a reactive root for the duration of query compilation.
///
/// Identity is inspected only while a plan is compiled. Compiled update paths
/// retain direct, typed handles and never perform an identity lookup.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SourceIdentity(*const ());

impl SourceIdentity {
    pub const fn from_ptr<T>(ptr: *const T) -> Self {
        Self(ptr.cast::<()>())
    }
}

struct RootDispatch<K, V> {
    active: bool,
    queued: VecDeque<MapDiff<K, V>>,
}

impl<K, V> Default for RootDispatch<K, V> {
    fn default() -> Self {
        Self {
            active: false,
            queued: VecDeque::new(),
        }
    }
}

struct RootRequirement {
    ordinal: usize,
    uses: usize,
    typed_sinks: Box<dyn Any>,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct RootKey {
    source: SourceIdentity,
    key: TypeId,
    value: TypeId,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct RelationshipKey {
    source: SourceIdentity,
    relation: TypeId,
}

type Activation = Box<dyn FnOnce() -> Vec<SubscriptionGuard>>;

trait PhysicalRelationshipBinding {
    fn bind(&self, key: RelationshipKey, indexes: &mut HashMap<RelationshipKey, Box<dyn Any>>);
}

struct TypedRelationshipBinding<T> {
    slot: DeferredPhysical<T>,
}

impl<T> PhysicalRelationshipBinding for TypedRelationshipBinding<T>
where
    T: Default + Send + Sync + 'static,
{
    fn bind(&self, key: RelationshipKey, indexes: &mut HashMap<RelationshipKey, Box<dyn Any>>) {
        let (index, maintains_index) = indexes
            .get(&key)
            .and_then(|index| index.downcast_ref::<Arc<Mutex<T>>>())
            .cloned()
            .map_or_else(
                || {
                    let index = Arc::new(Mutex::new(T::default()));
                    indexes.insert(key, Box::new(Arc::clone(&index)));
                    (index, true)
                },
                |index| (index, false),
            );
        let _already_bound = self.slot.inner.set(index).is_err();
        self.slot
            .maintains_index
            .store(maintains_index, Ordering::Release);
    }
}

/// A typed direct handle populated when the owning right subtree resolves to
/// its physical source during compilation.
pub struct DeferredPhysical<T> {
    inner: Arc<OnceLock<Arc<Mutex<T>>>>,
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
    pub(crate) fn read<R>(&self, read: impl FnOnce(&T) -> R) -> R {
        let index = self
            .inner
            .get_or_init(|| Arc::new(Mutex::new(T::default())));
        let index = index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        read(&index)
    }

    pub(crate) fn write<R>(&self, write: impl FnOnce(&mut T) -> R) -> R {
        let index = self
            .inner
            .get_or_init(|| Arc::new(Mutex::new(T::default())));
        let mut index = index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        write(&mut index)
    }

    pub(crate) fn maintains_index(&self) -> bool {
        self.maintains_index.load(Ordering::Acquire)
    }
}

/// Setup-time state shared by every node in one materialization.
#[derive(Default)]
pub struct CompileContext {
    roots: HashMap<RootKey, RootRequirement>,
    relationships: HashMap<RelationshipKey, usize>,
    relationship_indexes: HashMap<RelationshipKey, Box<dyn Any>>,
    activations: Vec<Activation>,
    relation_hint: Option<TypeId>,
    active_root_relation: Option<TypeId>,
    active_relationship_binding: Option<Box<dyn PhysicalRelationshipBinding>>,
}

impl CompileContext {
    /// Register a typed root entry point without activating its subscription.
    /// Repeated uses append to one root-boundary fanout and therefore install
    /// exactly one physical source subscription during activation.
    pub(crate) fn register_root<M, S>(
        &mut self,
        source: &M,
        identity: SourceIdentity,
        sink: S,
    ) -> usize
    where
        M: ReactiveMap + Clone,
        M::Key: CellValue + Hash + Eq,
        M::Value: CellValue,
        S: MapDiffSink<M::Key, M::Value>,
    {
        let root_key = RootKey {
            source: identity,
            key: TypeId::of::<M::Key>(),
            value: TypeId::of::<M::Value>(),
        };
        if let Some(relation) = self.active_root_relation {
            let relationship_key = RelationshipKey {
                source: identity,
                relation,
            };
            let uses = self.relationships.entry(relationship_key).or_default();
            *uses = uses.saturating_add(1);
            if let Some(binding) = &self.active_relationship_binding {
                binding.bind(relationship_key, &mut self.relationship_indexes);
            }
        }
        if let Some(requirement) = self.roots.get_mut(&root_key) {
            requirement.uses = requirement.uses.saturating_add(1);
            let ordinal = requirement.ordinal;
            if let Some(sinks) = requirement
                .typed_sinks
                .downcast_ref::<Arc<Mutex<Vec<BoxedMapDiffSink<M::Key, M::Value>>>>>()
            {
                sinks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(Arc::new(sink));
                return ordinal;
            }

            // Defensive correctness fallback if internal type erasure is ever
            // corrupted: keep the entry point live as an independent root.
            let source = source.clone();
            self.activations.push(Box::new(move || {
                vec![source.subscribe_diffs_reactive(move |diff| sink(diff))]
            }));
            return ordinal;
        }

        let next = self.roots.len();
        let first_sink: BoxedMapDiffSink<M::Key, M::Value> = Arc::new(sink);
        let sinks = Arc::new(Mutex::new(vec![first_sink]));
        self.roots.insert(
            root_key,
            RootRequirement {
                ordinal: next,
                uses: 1,
                typed_sinks: Box::new(Arc::clone(&sinks)),
            },
        );

        let source = source.clone();
        self.activations.push(Box::new(move || {
            let sinks = std::mem::take(
                &mut *sinks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            // One root event is a transaction across every compiled consumer.
            // Concurrent and reentrant notifications enqueue behind the active
            // event, so the relationship-index maintainer always runs before
            // readers for that exact diff and no event observes a later index.
            let dispatch = Arc::new(Mutex::new(RootDispatch::default()));
            let guard = source.subscribe_diffs_reactive(move |diff| {
                {
                    let mut state = dispatch
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.queued.push_back(diff.clone());
                    if state.active {
                        return;
                    }
                    state.active = true;
                }

                loop {
                    let next = {
                        let mut state = dispatch
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        match state.queued.pop_front() {
                            Some(diff) => diff,
                            None => {
                                state.active = false;
                                return;
                            }
                        }
                    };

                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        for sink in &sinks {
                            sink(&next);
                        }
                    }));
                    if let Err(payload) = result {
                        dispatch
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .active = false;
                        std::panic::resume_unwind(payload);
                    }
                }
            });
            vec![guard]
        }));
        next
    }

    pub(crate) fn activate(&mut self) -> Vec<SubscriptionGuard> {
        std::mem::take(&mut self.activations)
            .into_iter()
            .flat_map(|activate| activate())
            .collect()
    }

    pub(crate) fn activation_count(&self) -> usize {
        self.activations.len()
    }

    pub(crate) fn take_activations_since(&mut self, start: usize) -> Vec<Activation> {
        self.activations.split_off(start)
    }

    pub(crate) fn push_activation(&mut self, activation: Activation) {
        self.activations.push(activation);
    }

    pub(crate) fn with_relation_hint<Rel, T>(&mut self, compile: impl FnOnce(&mut Self) -> T) -> T
    where
        Rel: 'static,
    {
        let previous = self.relation_hint.replace(TypeId::of::<Rel>());
        let result = compile(self);
        self.relation_hint = previous;
        result
    }

    pub(crate) const fn take_relation_hint(&mut self) -> Option<TypeId> {
        self.relation_hint.take()
    }

    #[allow(clippy::unused_self)]
    pub(crate) fn prepare_relationship_index<T>(&self) -> DeferredPhysical<T> {
        DeferredPhysical::default()
    }

    pub(crate) fn with_root_relation_index<I, T>(
        &mut self,
        relation: TypeId,
        index: DeferredPhysical<I>,
        compile: impl FnOnce(&mut Self) -> T,
    ) -> T
    where
        I: Default + Send + Sync + 'static,
    {
        let previous_relation = self.active_root_relation.replace(relation);
        let previous_binding = self
            .active_relationship_binding
            .replace(Box::new(TypedRelationshipBinding { slot: index }));
        let result = compile(self);
        self.active_relationship_binding = previous_binding;
        self.active_root_relation = previous_relation;
        result
    }

    #[cfg(test)]
    pub(crate) fn root_count(&self) -> usize {
        self.roots.len()
    }

    #[cfg(test)]
    pub(crate) fn root_use_count(&self, identity: SourceIdentity) -> usize {
        self.roots
            .iter()
            .filter(|(key, _)| key.source == identity)
            .map(|(_, root)| root.uses)
            .sum()
    }

    #[cfg(test)]
    pub(crate) fn relationship_use_count<Rel: 'static>(&self, identity: SourceIdentity) -> usize {
        self.relationships
            .get(&RelationshipKey {
                source: identity,
                relation: TypeId::of::<Rel>(),
            })
            .copied()
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn physical_relationship_count(&self) -> usize {
        self.relationship_indexes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CellMap,
        map_query::compile_runtime_into,
        traits::{ForeignKeyRelation, IdFor, LeftJoinExt, MapValuesExt},
    };

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Parent;

    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    struct ParentId(u64);

    impl IdFor<Parent> for ParentId {
        type MapKey = Self;

        fn map_key(&self) -> Self::MapKey {
            self.clone()
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Child {
        parent: ParentId,
    }

    struct ParentChildren;

    impl ForeignKeyRelation for ParentChildren {
        type Parent = Parent;
        type Child = Child;
        type ForeignKey = ParentId;

        fn foreign_key(child: &Self::Child) -> Option<Self::ForeignKey> {
            Some(child.parent.clone())
        }
    }

    #[test]
    fn associated_runtime_defers_root_installation_until_connected() {
        use crate::map_query::{CompileQuery, QueryRuntime};

        let source = CellMap::<u64, u64>::new();
        let mut cx = CompileContext::default();
        let runtime = source.compile(&mut cx);

        assert_eq!(cx.root_count(), 0);
        assert_eq!(cx.activation_count(), 0);

        let guards = runtime.install_roots(&mut cx, |_| {});
        assert_eq!(cx.root_count(), 1);
        assert_eq!(guards.len(), 1);
    }

    #[test]
    fn repeated_physical_root_is_interned_once() {
        let source = CellMap::<u64, u64>::new();
        let identity = SourceIdentity::from_ptr(std::sync::Arc::as_ptr(&source.inner));
        let mut cx = CompileContext::default();
        assert_eq!(cx.register_root(&source, identity, |_| {}), 0);
        assert_eq!(cx.register_root(&source, identity, |_| {}), 0);
        assert_eq!(cx.root_count(), 1);
        assert_eq!(cx.activate().len(), 1);
    }

    #[test]
    fn compilation_recognizes_a_root_reused_by_two_joins() {
        let left = CellMap::<u64, u64>::new();
        let repeated = CellMap::<u64, u64>::new();
        let repeated_identity = SourceIdentity::from_ptr(std::sync::Arc::as_ptr(&repeated.inner));
        let plan = left
            .left_join(repeated.clone())
            .map_values(|_, (value, _)| *value)
            .left_join(repeated);

        let mut cx = CompileContext::default();
        let mut guards =
            compile_runtime_into(plan, &mut cx, |_: &crate::cell_map::MapDiff<_, _>| {});

        assert_eq!(cx.root_count(), 2);
        assert_eq!(cx.root_use_count(repeated_identity), 2);
        guards.extend(cx.activate());
        assert_eq!(guards.len(), 2);
        drop(guards);
    }

    #[test]
    fn relationship_identity_is_scoped_to_each_join_right_root() {
        let parents = CellMap::<ParentId, Parent>::new();
        let children = CellMap::<u64, Child>::new();
        let children_identity = SourceIdentity::from_ptr(Arc::as_ptr(&children.inner));
        let plan = parents
            .left_join_fk::<ParentChildren, _>(children.clone())
            .map_joined_values(|_, parent, _| parent.clone())
            .left_join_fk::<ParentChildren, _>(children);

        let mut cx = CompileContext::default();
        let mut guards = compile_runtime_into(plan, &mut cx, |_| {});

        assert_eq!(
            cx.relationship_use_count::<ParentChildren>(children_identity),
            2
        );
        assert_eq!(cx.physical_relationship_count(), 1);
        guards.extend(cx.activate());
        assert_eq!(guards.len(), 2);
    }

    #[test]
    fn reentrant_root_changes_wait_for_the_active_fanout_transaction() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let source = CellMap::<u64, u64>::new();
        let identity = SourceIdentity::from_ptr(Arc::as_ptr(&source.inner));
        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let armed = Arc::new(AtomicBool::new(false));
        let triggered = Arc::new(AtomicBool::new(false));
        let mut cx = CompileContext::default();

        let first_events = Arc::clone(&events);
        let first_source = source.clone();
        let first_armed = Arc::clone(&armed);
        let first_triggered = Arc::clone(&triggered);
        cx.register_root(&source, identity, move |diff| {
            first_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("a:{diff:?}"));
            if first_armed.load(Ordering::Acquire) && !first_triggered.swap(true, Ordering::AcqRel)
            {
                first_source.insert(2, 20);
            }
        });
        let second_events = Arc::clone(&events);
        cx.register_root(&source, identity, move |diff| {
            second_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("b:{diff:?}"));
        });

        let guards = cx.activate();
        events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        armed.store(true, Ordering::Release);
        source.insert(1, 10);

        let events = events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(events.len(), 4);
        assert!(events[0].starts_with("a:"));
        assert!(events[1].starts_with("b:"));
        assert_eq!(&events[0][2..], &events[1][2..]);
        assert!(events[2].starts_with("a:"));
        assert!(events[3].starts_with("b:"));
        assert_eq!(&events[2][2..], &events[3][2..]);
        assert_ne!(&events[0][2..], &events[2][2..]);
        drop(guards);
    }

    #[test]
    fn repeated_relationship_assigns_exactly_one_index_maintainer() {
        let source = CellMap::<u64, u64>::new();
        let identity = SourceIdentity::from_ptr(Arc::as_ptr(&source.inner));
        let mut cx = CompileContext::default();
        let first = cx.prepare_relationship_index::<Vec<u64>>();
        let second = cx.prepare_relationship_index::<Vec<u64>>();

        cx.with_root_relation_index(TypeId::of::<ParentChildren>(), first.clone(), |cx| {
            cx.register_root(&source, identity, |_| {});
        });
        cx.with_root_relation_index(TypeId::of::<ParentChildren>(), second.clone(), |cx| {
            cx.register_root(&source, identity, |_| {});
        });

        assert!(first.maintains_index());
        assert!(!second.maintains_index());
        assert_eq!(cx.physical_relationship_count(), 1);
    }
}
