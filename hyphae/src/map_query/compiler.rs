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
mod tests;
