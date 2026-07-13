use std::{collections::HashSet, panic::Location, sync::Arc};

use uuid::Uuid;

/// Type-erased trait for dependency graph introspection.
///
/// This enables recursive traversal of the dependency tree regardless of
/// the concrete value types of each cell.
pub trait DepNode: Send + Sync {
    fn id(&self) -> Uuid;
    fn name(&self) -> Option<String>;
    /// Returns the dependencies of this node.
    fn deps(&self) -> Vec<Arc<dyn DepNode>>;

    /// The scheduler's per-node height cache slot, if any. Cells expose their
    /// `CellInner::height_cache`; other nodes default to `None` and are
    /// recomputed each time. Lets the scheduler memoize propagation height
    /// (`1 + max(dep.height)`) across ticks, invalidated by the topology epoch.
    #[cfg(feature = "scheduler")]
    fn height_cache(&self) -> Option<&std::sync::atomic::AtomicU64> {
        None
    }

    /// The node's per-node **height epoch** — bumped whenever an edge change in
    /// this node's transitive-dependency cone could alter its height (see
    /// [`crate::cell::invalidate_height_cone`]). The scheduler tags each cached
    /// height with the epoch it was computed under and recomputes when they
    /// differ, so invalidation is localized to the affected subgraph instead of
    /// flushing every cached height in the process on any edge change. Cells
    /// expose `CellInner::height_epoch`; other nodes default to `None` and are
    /// recomputed each read.
    #[cfg(feature = "scheduler")]
    fn height_epoch(&self) -> Option<&std::sync::atomic::AtomicU64> {
        None
    }

    /// Register `dep` as a node whose height depends (transitively) on this one,
    /// so a later edge change here invalidates its cached height. Called when a
    /// cell takes ownership of a subscription guard whose source is this node.
    /// Cells push into `CellInner::height_dependents`; other nodes ignore it.
    #[cfg(feature = "scheduler")]
    fn add_height_dependent(&self, _dep: std::sync::Weak<dyn crate::cell::HeightInvalidate>) {}

    /// Whether the scheduler should skip last-write-wins coalescing for this
    /// node — enqueuing every notify as a distinct height-ordered op so event
    /// semantics (scan/pairwise/merge and hand-rolled stateful maps) survive a
    /// batch. Cells expose their per-cell flag; other nodes default to `false`.
    #[cfg(feature = "scheduler")]
    fn no_coalesce(&self) -> bool {
        false
    }

    /// Returns the number of active subscribers to this node.
    fn subscriber_count(&self) -> usize {
        0
    }

    /// Returns the number of subscription guards owned by this node.
    fn owned_count(&self) -> usize {
        0
    }

    /// Returns the Debug-formatted current value, if available.
    fn value_debug(&self) -> Option<String> {
        None
    }

    /// Returns the source location where this cell was created.
    fn caller(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn display_name(&self) -> String {
        self.name()
            .unwrap_or_else(|| format!("Cell({})", &self.id().to_string()[..8]))
    }

    fn dependency_count(&self) -> usize {
        self.deps().len()
    }

    fn has_dependencies(&self) -> bool {
        !self.deps().is_empty()
    }

    fn dependency_tree(&self) -> String
    where
        Self: Sized,
    {
        fn write_tree(
            node: &dyn DepNode,
            out: &mut String,
            prefix: &str,
            visited: &mut HashSet<Uuid>,
        ) {
            use std::fmt::Write;
            let _ = writeln!(out, "{}", node.display_name());

            if visited.contains(&node.id()) {
                let _ = writeln!(out, "{}(cycle)", prefix);
                return;
            }
            visited.insert(node.id());

            let deps = node.deps();
            for (i, dep) in deps.iter().enumerate() {
                let is_last = i == deps.len() - 1;
                let connector = if is_last { "└─ " } else { "├─ " };
                let child_prefix = if is_last {
                    format!("{}   ", prefix)
                } else {
                    format!("{}│  ", prefix)
                };

                let _ = write!(out, "{}{}", prefix, connector);
                write_tree(dep.as_ref(), out, &child_prefix, visited);
            }
        }

        let mut out = String::new();
        write_tree(self, &mut out, "", &mut HashSet::new());
        out
    }

    fn print_dependency_tree(&self)
    where
        Self: Sized,
    {
        print!("{}", self.dependency_tree());
    }
}
