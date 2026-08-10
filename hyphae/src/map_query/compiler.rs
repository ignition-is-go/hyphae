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

/// Setup-time state shared by every node in one materialization.
#[derive(Default)]
pub struct CompileContext {
    roots: HashMap<RootKey, RootRequirement>,
    relationships: HashMap<RelationshipKey, usize>,
    activations: Vec<Activation>,
    relation_hint: Option<TypeId>,
    active_root_relation: Option<TypeId>,
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
            let uses = self
                .relationships
                .entry(RelationshipKey {
                    source: identity,
                    relation,
                })
                .or_default();
            *uses = uses.saturating_add(1);
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
            let guard = source.subscribe_diffs_reactive(move |diff| {
                for sink in &sinks {
                    sink(diff);
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

    pub(crate) fn with_root_relation<T>(
        &mut self,
        relation: TypeId,
        compile: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let previous = self.active_root_relation.replace(relation);
        let result = compile(self);
        self.active_root_relation = previous;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CellMap,
        map_query::CompileQuery,
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
        let mut guards = plan.compile_into(&mut cx, |_: &crate::cell_map::MapDiff<_, _>| {});

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
        let mut guards = plan.compile_into(&mut cx, |_| {});

        assert_eq!(
            cx.relationship_use_count::<ParentChildren>(children_identity),
            2
        );
        guards.extend(cx.activate());
        assert_eq!(guards.len(), 2);
    }
}
