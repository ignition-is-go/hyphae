use std::{
    any::{Any, TypeId},
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex},
};

use crate::{
    subscription::SubscriptionGuard,
    traits::{CellValue, reactive_map::ReactiveMap},
};

use super::BoxedMapDiffSink;

mod dispatch;
mod registry;

use dispatch::{QueryDispatch, dispatch_query_root};
use registry::{
    PhysicalRelationshipBinding, RelationshipKey, RootKey, RootRequirement,
    TypedRelationshipBinding,
};

pub use dispatch::{QUERY_POISONED_MESSAGE, QueryPoison};
pub use registry::DeferredPhysical;

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

type Activation = Box<dyn FnOnce() -> Vec<SubscriptionGuard>>;

/// Setup-time state shared by every node in one materialization.
#[derive(Default)]
pub struct CompileContext {
    query_dispatch: Arc<Mutex<QueryDispatch>>,
    query_poison: QueryPoison,
    roots: HashMap<RootKey, RootRequirement>,
    relationships: HashMap<RelationshipKey, usize>,
    relationship_indexes: HashMap<RelationshipKey, Box<dyn Any>>,
    activations: Vec<Activation>,
    relation_hint: Option<TypeId>,
    active_root_relation: Option<TypeId>,
    active_relationship_binding: Option<Box<dyn PhysicalRelationshipBinding>>,
}

impl CompileContext {
    pub(crate) fn query_poison(&self) -> QueryPoison {
        self.query_poison.clone()
    }

    /// Register a typed root entry point without activating its subscription.
    /// Repeated uses append to one root-boundary fanout and therefore install
    /// exactly one physical source subscription during activation.
    pub(crate) fn register_root<M>(
        &mut self,
        source: &M,
        identity: SourceIdentity,
        sink: crate::map_query::BoxedMapDiffSink<M::Key, M::Value>,
    ) -> usize
    where
        M: ReactiveMap + Clone,
        M::Key: CellValue + Hash + Eq,
        M::Value: CellValue,
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
            let sinks = requirement
                .typed_sinks
                .downcast_ref::<Arc<Mutex<Vec<BoxedMapDiffSink<M::Key, M::Value>>>>>();
            assert!(
                sinks.is_some(),
                "compiler invariant violated: root sink type mismatch"
            );
            let Some(sinks) = sinks else {
                return requirement.ordinal;
            };
            requirement.uses = requirement.uses.saturating_add(1);
            let ordinal = requirement.ordinal;
            sinks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(sink);
            return ordinal;
        }

        let next = self.roots.len();
        let first_sink: BoxedMapDiffSink<M::Key, M::Value> = sink;
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
        let dispatch = Arc::clone(&self.query_dispatch);
        let poison = self.query_poison.clone();
        self.activations.push(Box::new(move || {
            let sinks = Arc::new(std::mem::take(
                &mut *sinks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            ));
            // The query-wide gate makes the complete physical-root fanout one
            // transaction. A different root cannot publish between a shared
            // relationship's maintainer and observers.
            let guard = source.subscribe_diffs_reactive(move |diff| {
                dispatch_query_root(&dispatch, &poison, &sinks, diff);
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
        cell_map::MapDiff,
        map_query::compile_runtime_into,
        traits::{ForeignKeyRelation, IdFor, LeftJoinExt, MapValuesExt},
    };
    use std::sync::atomic::{AtomicBool, Ordering};

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
    fn deferred_physical_admits_two_readers_while_writer_waits() {
        use std::{
            sync::{Barrier, mpsc},
            thread,
            time::Duration,
        };

        let index = DeferredPhysical::<usize>::default();
        let readers_release = Arc::new(Barrier::new(3));
        let (reader_acquired_tx, reader_acquired_rx) = mpsc::channel();
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let index = index.clone();
                let readers_release = Arc::clone(&readers_release);
                let reader_acquired_tx = reader_acquired_tx.clone();
                thread::spawn(move || {
                    let _read = index.acquire_read();
                    assert!(reader_acquired_tx.send(()).is_ok());
                    readers_release.wait();
                })
            })
            .collect();
        drop(reader_acquired_tx);

        assert_eq!(
            reader_acquired_rx.recv_timeout(Duration::from_secs(1)),
            Ok(())
        );
        assert_eq!(
            reader_acquired_rx.recv_timeout(Duration::from_secs(1)),
            Ok(())
        );

        let (writer_attempting_tx, writer_attempting_rx) = mpsc::channel();
        let (writer_acquired_tx, writer_acquired_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            assert!(writer_attempting_tx.send(()).is_ok());
            index.write(|value| {
                *value += 1;
                assert!(writer_acquired_tx.send(()).is_ok());
            });
        });
        assert_eq!(
            writer_attempting_rx.recv_timeout(Duration::from_secs(1)),
            Ok(())
        );
        assert!(
            writer_acquired_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );

        readers_release.wait();
        assert_eq!(
            writer_acquired_rx.recv_timeout(Duration::from_secs(1)),
            Ok(())
        );
        for reader in readers {
            assert!(reader.join().is_ok());
        }
        assert!(writer.join().is_ok());
    }

    #[test]
    fn associated_runtime_defers_root_installation_until_connected() {
        use crate::map_query::{CompileQuery, QueryRuntime};

        let source = CellMap::<u64, u64>::new();
        let mut cx = CompileContext::default();
        let runtime = source.compile(&mut cx);

        assert_eq!(cx.root_count(), 0);
        assert_eq!(cx.activation_count(), 0);

        let guards = runtime.install_roots(&mut cx, Arc::new(|_| {}));
        assert_eq!(cx.root_count(), 1);
        assert_eq!(guards.len(), 1);
    }

    #[test]
    fn repeated_physical_root_is_interned_once() {
        let source = CellMap::<u64, u64>::new();
        let identity = SourceIdentity::from_ptr(std::sync::Arc::as_ptr(&source.inner));
        let mut cx = CompileContext::default();
        assert_eq!(cx.register_root(&source, identity, Arc::new(|_| {})), 0);
        assert_eq!(cx.register_root(&source, identity, Arc::new(|_| {})), 0);
        assert_eq!(cx.root_count(), 1);
        assert_eq!(cx.activate().len(), 1);
    }

    #[test]
    #[should_panic(expected = "compiler invariant violated: root sink type mismatch")]
    fn register_root_rejects_a_corrupted_erased_sink_type() {
        let source = CellMap::<u64, u64>::new();
        let identity = SourceIdentity::from_ptr(Arc::as_ptr(&source.inner));
        let root_key = RootKey {
            source: identity,
            key: TypeId::of::<u64>(),
            value: TypeId::of::<u64>(),
        };
        let mut cx = CompileContext::default();
        cx.roots.insert(
            root_key,
            RootRequirement {
                ordinal: 0,
                uses: 1,
                typed_sinks: Box::new(()),
            },
        );

        cx.register_root(&source, identity, Arc::new(|_| {}));
    }

    #[test]
    #[should_panic(expected = "compiler invariant violated: relationship index type mismatch")]
    fn relationship_binding_rejects_a_corrupted_erased_index_type() {
        let key = RelationshipKey {
            source: SourceIdentity::from_ptr(std::ptr::dangling::<u8>()),
            relation: TypeId::of::<ParentChildren>(),
        };
        let binding = TypedRelationshipBinding::<Vec<u64>> {
            slot: DeferredPhysical::default(),
        };
        let mut indexes: HashMap<RelationshipKey, Box<dyn Any>> = HashMap::new();
        indexes.insert(
            key,
            Box::new(Arc::new(parking_lot::RwLock::new(Vec::<String>::new()))),
        );

        binding.bind(key, &mut indexes);
    }

    #[test]
    #[should_panic(expected = "compiler invariant violated: physical index slot already bound")]
    fn relationship_binding_rejects_an_already_bound_slot() {
        let key = RelationshipKey {
            source: SourceIdentity::from_ptr(std::ptr::dangling::<u8>()),
            relation: TypeId::of::<ParentChildren>(),
        };
        let slot = DeferredPhysical::<Vec<u64>>::default();
        slot.write(|index| index.push(1));
        let binding = TypedRelationshipBinding { slot };
        let mut indexes = HashMap::new();

        binding.bind(key, &mut indexes);
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
        let mut guards = compile_runtime_into(
            plan,
            &mut cx,
            Arc::new(|_: &crate::cell_map::MapDiff<_, _>| {}),
        );

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
        let mut guards = compile_runtime_into(plan, &mut cx, Arc::new(|_| {}));

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
        let events = Arc::new(Mutex::new(Vec::<(char, String)>::new()));
        let armed = Arc::new(AtomicBool::new(false));
        let triggered = Arc::new(AtomicBool::new(false));
        let mut cx = CompileContext::default();

        let first_events = Arc::clone(&events);
        let first_source = source.clone();
        let first_armed = Arc::clone(&armed);
        let first_triggered = Arc::clone(&triggered);
        cx.register_root(
            &source,
            identity,
            Arc::new(move |diff| {
                first_events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(('a', format!("{diff:?}")));
                if first_armed.load(Ordering::Acquire)
                    && !first_triggered.swap(true, Ordering::AcqRel)
                {
                    first_source.insert(2, 20);
                }
            }),
        );
        let second_events = Arc::clone(&events);
        cx.register_root(
            &source,
            identity,
            Arc::new(move |diff| {
                second_events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(('b', format!("{diff:?}")));
            }),
        );

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
        if let [first_a, first_b, second_a, second_b] = events.as_slice() {
            assert_eq!(first_a.0, 'a');
            assert_eq!(first_b.0, 'b');
            assert_eq!(first_a.1, first_b.1);
            assert_eq!(second_a.0, 'a');
            assert_eq!(second_b.0, 'b');
            assert_eq!(second_a.1, second_b.1);
            assert_ne!(first_a.1, second_a.1);
        } else {
            assert_eq!(events.len(), 4, "incomplete transactions: {events:?}");
        }
        drop(events);
        drop(guards);
    }

    #[test]
    fn differently_typed_roots_share_one_fifo_transaction_gate() {
        let first = CellMap::<u64, u64>::new();
        let second = CellMap::<String, u64>::new();
        let first_identity = SourceIdentity::from_ptr(Arc::as_ptr(&first.inner));
        let second_identity = SourceIdentity::from_ptr(Arc::as_ptr(&second.inner));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let armed = Arc::new(AtomicBool::new(false));
        let mut cx = CompileContext::default();

        let first_observed = Arc::clone(&observed);
        let first_release = Arc::clone(&release_rx);
        let first_armed = Arc::clone(&armed);
        cx.register_root(
            &first,
            first_identity,
            Arc::new(move |_| {
                first_observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push("a");
                if first_armed.load(Ordering::Acquire) {
                    assert!(entered_tx.send(()).is_ok());
                    assert!(
                        first_release
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .recv()
                            .is_ok()
                    );
                }
            }),
        );
        let second_observed = Arc::clone(&observed);
        cx.register_root(
            &second,
            second_identity,
            Arc::new(move |_| {
                second_observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push("b");
            }),
        );
        let guards = cx.activate();
        observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        armed.store(true, Ordering::Release);

        let first_thread = first.clone();
        let active = std::thread::spawn(move || first_thread.insert(1, 10));
        assert!(entered_rx.recv().is_ok());
        second.insert("two".to_owned(), 20);
        assert_eq!(
            *observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!["a"]
        );
        assert!(release_tx.send(()).is_ok());
        assert!(active.join().is_ok());
        assert_eq!(
            *observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!["a", "b"]
        );
        drop(guards);
    }

    #[test]
    fn synchronous_different_root_mutation_is_deferred_until_a_finishes() {
        let first = CellMap::<u64, u64>::new();
        let second = CellMap::<String, String>::new();
        let first_identity = SourceIdentity::from_ptr(Arc::as_ptr(&first.inner));
        let second_identity = SourceIdentity::from_ptr(Arc::as_ptr(&second.inner));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let armed = Arc::new(AtomicBool::new(false));
        let mut cx = CompileContext::default();

        let first_observed = Arc::clone(&observed);
        let first_second = second.clone();
        let first_armed = Arc::clone(&armed);
        cx.register_root(
            &first,
            first_identity,
            Arc::new(move |_| {
                first_observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push("a-enter");
                if first_armed.load(Ordering::Acquire) {
                    first_second.insert("key".to_owned(), "value".to_owned());
                }
                first_observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push("a-exit");
            }),
        );
        let second_observed = Arc::clone(&observed);
        cx.register_root(
            &second,
            second_identity,
            Arc::new(move |_| {
                second_observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push("b");
            }),
        );
        let guards = cx.activate();
        observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        armed.store(true, Ordering::Release);

        first.insert(1, 10);

        assert_eq!(
            *observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!["a-enter", "a-exit", "b"]
        );
        drop(guards);
    }

    #[test]
    fn reentrant_different_root_waits_for_complete_repeated_fanout() {
        let first = CellMap::<u64, u64>::new();
        let second = CellMap::<String, u64>::new();
        let first_identity = SourceIdentity::from_ptr(Arc::as_ptr(&first.inner));
        let second_identity = SourceIdentity::from_ptr(Arc::as_ptr(&second.inner));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let armed = Arc::new(AtomicBool::new(false));
        let mut cx = CompileContext::default();

        let maintainer_observed = Arc::clone(&observed);
        let maintainer_second = second.clone();
        let maintainer_armed = Arc::clone(&armed);
        cx.register_root(
            &first,
            first_identity,
            Arc::new(move |_| {
                maintainer_observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push("maintainer");
                if maintainer_armed.load(Ordering::Acquire) {
                    maintainer_second.insert("queued".to_owned(), 1);
                }
            }),
        );
        let observer_observed = Arc::clone(&observed);
        cx.register_root(
            &first,
            first_identity,
            Arc::new(move |_| {
                observer_observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push("observer");
            }),
        );
        let second_observed = Arc::clone(&observed);
        cx.register_root(
            &second,
            second_identity,
            Arc::new(move |_| {
                second_observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push("second-root");
            }),
        );
        let guards = cx.activate();
        observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        armed.store(true, Ordering::Release);

        first.insert(1, 10);

        assert_eq!(
            *observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!["maintainer", "observer", "second-root"]
        );
        drop(guards);
    }

    #[derive(Debug)]
    struct CloneTracked {
        clones: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Clone for CloneTracked {
        fn clone(&self) -> Self {
            self.clones.fetch_add(1, Ordering::Relaxed);
            Self {
                clones: Arc::clone(&self.clones),
            }
        }
    }

    impl PartialEq for CloneTracked {
        fn eq(&self, other: &Self) -> bool {
            Arc::ptr_eq(&self.clones, &other.clones)
        }
    }

    #[test]
    fn uncontended_query_dispatch_borrows_the_source_diff() {
        let clones = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let diff = MapDiff::Insert {
            key: 1_u64,
            value: CloneTracked {
                clones: Arc::clone(&clones),
            },
        };
        let dispatch = Mutex::new(QueryDispatch::default());
        let poison = QueryPoison::default();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink_calls = Arc::clone(&calls);
        let sinks: Arc<Vec<BoxedMapDiffSink<u64, CloneTracked>>> =
            Arc::new(vec![Arc::new(move |_| {
                sink_calls.fetch_add(1, Ordering::Relaxed);
            })]);

        dispatch_query_root(&dispatch, &poison, &sinks, &diff);

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(clones.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn concurrent_query_event_queues_behind_active_fanout() {
        let dispatch = Arc::new(Mutex::new(QueryDispatch::default()));
        let poison = QueryPoison::default();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let sink_observed = Arc::clone(&observed);
        let sink_release = Arc::clone(&release_rx);
        let sinks: Arc<Vec<BoxedMapDiffSink<u64, u64>>> = Arc::new(vec![Arc::new(move |diff| {
            let MapDiff::Insert { key, .. } = diff else {
                assert!(
                    matches!(diff, MapDiff::Insert { .. }),
                    "unexpected test diff: {diff:?}"
                );
                return;
            };
            sink_observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(*key);
            if *key == 1 {
                assert!(
                    entered_tx.send(()).is_ok(),
                    "test receiver must remain live"
                );
                assert!(
                    sink_release
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .recv()
                        .is_ok(),
                    "test sender must remain live"
                );
            }
        })]);
        let first_dispatch = Arc::clone(&dispatch);
        let first_poison = poison.clone();
        let first_sinks = Arc::clone(&sinks);
        let first = std::thread::spawn(move || {
            dispatch_query_root(
                &first_dispatch,
                &first_poison,
                &first_sinks,
                &MapDiff::Insert { key: 1, value: 10 },
            );
        });

        assert!(entered_rx.recv().is_ok(), "first fanout must start");
        dispatch_query_root(
            &dispatch,
            &poison,
            &sinks,
            &MapDiff::Insert { key: 2, value: 20 },
        );
        assert!(
            release_tx.send(()).is_ok(),
            "blocked fanout must remain live"
        );
        assert!(first.join().is_ok(), "fanout thread must finish");

        assert_eq!(
            *observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![1, 2]
        );
    }

    #[test]
    fn panic_clears_queued_query_events_and_recovers() {
        let dispatch = Arc::new(Mutex::new(QueryDispatch::default()));
        let poison = QueryPoison::default();
        let should_panic = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let sink_should_panic = Arc::clone(&should_panic);
        let sink_calls = Arc::clone(&calls);
        let sink_release = Arc::clone(&release_rx);
        let sinks: Arc<Vec<BoxedMapDiffSink<u64, u64>>> = Arc::new(vec![Arc::new(move |diff| {
            sink_calls.fetch_add(1, Ordering::Relaxed);
            let must_panic = sink_should_panic.swap(false, Ordering::AcqRel);
            if must_panic {
                assert!(entered_tx.send(()).is_ok());
                assert!(
                    sink_release
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .recv()
                        .is_ok()
                );
            }
            assert!(!must_panic, "first event panic: {diff:?}");
        })]);

        let panic_dispatch = Arc::clone(&dispatch);
        let panic_poison = poison.clone();
        let panic_sinks = Arc::clone(&sinks);
        let panicking = std::thread::spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                dispatch_query_root(
                    &panic_dispatch,
                    &panic_poison,
                    &panic_sinks,
                    &MapDiff::Insert { key: 1, value: 10 },
                );
            }))
        });
        assert!(entered_rx.recv().is_ok());
        dispatch_query_root(
            &dispatch,
            &poison,
            &sinks,
            &MapDiff::Insert { key: 2, value: 20 },
        );
        assert!(release_tx.send(()).is_ok());
        assert!(panicking.join().is_ok_and(|result| result.is_err()));

        // The queued second event belonged to the partially committed
        // transaction and was discarded. A fresh third event is accepted.
        dispatch_query_root(
            &dispatch,
            &poison,
            &sinks,
            &MapDiff::Insert { key: 3, value: 30 },
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        let state = dispatch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!state.active);
        assert!(state.queued.is_empty());
        drop(state);
    }

    // Deep public-query composition is covered by
    // `tests/arbitrary_n_public_join_region.rs`. Keeping another eight-stage
    // instantiation in the monolithic lib-test crate multiplies downstream
    // codegen and defeats the bounded-compilation resource contract.

    #[test]
    fn repeated_relationship_assigns_exactly_one_index_maintainer() {
        let source = CellMap::<u64, u64>::new();
        let identity = SourceIdentity::from_ptr(Arc::as_ptr(&source.inner));
        let mut cx = CompileContext::default();
        let first = cx.prepare_relationship_index::<Vec<u64>>();
        let second = cx.prepare_relationship_index::<Vec<u64>>();

        cx.with_root_relation_index(TypeId::of::<ParentChildren>(), first.clone(), |cx| {
            cx.register_root(&source, identity, Arc::new(|_| {}));
        });
        cx.with_root_relation_index(TypeId::of::<ParentChildren>(), second.clone(), |cx| {
            cx.register_root(&source, identity, Arc::new(|_| {}));
        });

        assert!(first.maintains_index());
        assert!(!second.maintains_index());
        assert_eq!(cx.physical_relationship_count(), 1);
    }
}
