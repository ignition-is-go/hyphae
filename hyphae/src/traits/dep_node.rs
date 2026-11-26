use std::{collections::HashSet, sync::Arc};

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

    /// Propagation height: strictly greater than the height of every node
    /// this one depends on. Cells keep it as a maintained atomic (see
    /// `crate::cell::raise_height`); other nodes derive it from `deps()`.
    #[doc(hidden)]
    fn height(&self) -> u64 {
        self.deps()
            .iter()
            .map(|dep| dep.height().saturating_add(1))
            .max()
            .unwrap_or(0)
    }

    /// Register `dep` as a node whose height must stay above this one's, so a
    /// later raise of this node's height also raises `dep`. Called when a cell
    /// takes ownership of a subscription guard whose source is this node.
    /// Cells push into `CellInner::height_dependents`; other nodes ignore it.
    fn add_height_dependent(&self, _dep: std::sync::Weak<dyn crate::cell::HeightNode>) {}

    /// Whether the scheduler should skip last-write-wins coalescing for this
    /// node — enqueuing every notify as a distinct height-ordered op so event
    /// semantics (scan/pairwise/merge and hand-rolled stateful maps) survive a
    /// batch. Cells expose their per-cell flag; other nodes default to `false`.
    fn no_coalesce(&self) -> bool {
        false
    }

    /// Check that a cell of scheduling domain `reader` may depend on this node.
    /// `Ok(true)` means the dependency crosses the boundary from the root
    /// domain into a child. Nodes that are not cells belong to no domain and
    /// accept every reader.
    ///
    /// # Errors
    ///
    /// Returns the [`DomainError`](crate::scheduler::DomainError) describing a
    /// dependency that points from root to a child or between two children.
    fn check_reader(&self, _reader: u64) -> Result<bool, crate::scheduler::DomainError> {
        Ok(false)
    }

    /// Returns the number of active subscribers to this node.
    fn subscriber_count(&self) -> usize {
        0
    }

    /// Returns the number of subscription guards owned by this node.
    fn owned_count(&self) -> usize {
        0
    }

    fn display_name(&self) -> String {
        self.name().unwrap_or_else(|| {
            let short_id: String = self.id().to_string().chars().take(8).collect();
            format!("Cell({short_id})")
        })
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
                let _ = writeln!(out, "{prefix}(cycle)");
                return;
            }
            visited.insert(node.id());

            let deps = node.deps();
            for (i, dep) in deps.iter().enumerate() {
                let is_last = i == deps.len().saturating_sub(1);
                let connector = if is_last { "└─ " } else { "├─ " };
                let child_prefix = if is_last {
                    format!("{prefix}   ")
                } else {
                    format!("{prefix}│  ")
                };

                let _ = write!(out, "{prefix}{connector}");
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
