use std::collections::HashSet;
use std::sync::Arc;
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
