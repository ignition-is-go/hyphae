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

    /// Cache slot for this node's height, when it has one.
    #[doc(hidden)]
    fn height_cache(&self) -> Option<&std::sync::atomic::AtomicU64> {
        None
    }

    /// The epoch that validates [`height_cache`](Self::height_cache).
    #[doc(hidden)]
    fn height_epoch(&self) -> Option<&std::sync::atomic::AtomicU64> {
        None
    }

    /// Register a node whose height depends on this one.
    #[doc(hidden)]
    fn add_dependent(&self, _dependent: std::sync::Weak<dyn crate::settle::Node>) {}

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
