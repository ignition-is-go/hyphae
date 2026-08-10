use std::collections::HashMap;

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

#[derive(Clone, Copy)]
struct RootRequirement {
    ordinal: usize,
    uses: usize,
}

/// Setup-time state shared by every node in one materialization.
#[derive(Default)]
pub struct CompileContext {
    roots: HashMap<SourceIdentity, RootRequirement>,
}

impl CompileContext {
    /// Intern a physical root and return its plan-local ordinal.
    pub(crate) fn intern_root(&mut self, identity: SourceIdentity) -> usize {
        let next = self.roots.len();
        let requirement = self.roots.entry(identity).or_insert(RootRequirement {
            ordinal: next,
            uses: 0,
        });
        requirement.uses = requirement.uses.saturating_add(1);
        requirement.ordinal
    }

    #[cfg(test)]
    pub(crate) fn root_count(&self) -> usize {
        self.roots.len()
    }

    #[cfg(test)]
    pub(crate) fn root_use_count(&self, identity: SourceIdentity) -> usize {
        self.roots.get(&identity).map_or(0, |root| root.uses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CellMap,
        map_query::MapQueryInstall,
        traits::{LeftJoinExt, MapValuesExt},
    };

    #[test]
    fn repeated_physical_root_is_interned_once() {
        let value = 1_u8;
        let identity = SourceIdentity::from_ptr(std::ptr::from_ref(&value));
        let mut cx = CompileContext::default();
        assert_eq!(cx.intern_root(identity), 0);
        assert_eq!(cx.intern_root(identity), 0);
        assert_eq!(cx.root_count(), 1);
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
        let guards = plan.install(&mut cx, |_: &crate::cell_map::MapDiff<_, _>| {});

        assert_eq!(cx.root_count(), 2);
        assert_eq!(cx.root_use_count(repeated_identity), 2);
        drop(guards);
    }
}
