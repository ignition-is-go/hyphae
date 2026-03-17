//! Global cell registry for the inspector feature.
//!
//! Tracks all live cells and their ownership relationships using lock-free data structures.

use std::sync::{OnceLock, Weak};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::traits::DepNode;

const MAX_VALUE_LEN: usize = 4096;

/// Snapshot of a single cell's state, suitable for serialization and transmission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellSnapshot {
    pub id: Uuid,
    pub name: Option<String>,
    pub display_name: String,
    pub subscriber_count: usize,
    pub owned_count: usize,
    pub dep_ids: Vec<Uuid>,
    /// The id of the cell that owns this cell (via `Cell::own()`), if any.
    pub owner_id: Option<Uuid>,
    /// Debug-formatted current value.
    #[serde(default)]
    pub value: Option<String>,
    /// Source location where this cell was created (file:line:col).
    #[serde(default)]
    pub caller: Option<String>,
}

/// Global registry of all live hyphae cells.
///
/// Uses `DashMap` for lock-free concurrent access. Cells register themselves on creation
/// and deregister on drop. Ownership relationships (from `Cell::own()`) are tracked
/// separately so the inspector can identify root cells (those with no owner).
pub struct CellRegistry {
    /// All live cells, stored as weak references so the registry doesn't prevent GC.
    cells: DashMap<Uuid, Weak<dyn DepNode>>,
    /// Ownership: child_id → parent_id. Populated when `Cell::own(guard)` is called.
    ownership: DashMap<Uuid, Uuid>,
}

impl CellRegistry {
    fn new() -> Self {
        Self {
            cells: DashMap::new(),
            ownership: DashMap::new(),
        }
    }

    /// Register a cell in the registry.
    pub fn register(&self, id: Uuid, weak: Weak<dyn DepNode>) {
        self.cells.insert(id, weak);
    }

    /// Deregister a cell from the registry.
    pub fn deregister(&self, id: &Uuid) {
        self.cells.remove(id);
        self.ownership.remove(id);
    }

    /// Record that `parent_id` owns `child_id` (the child's subscription source).
    pub fn mark_owned(&self, child_id: Uuid, parent_id: Uuid) {
        self.ownership.insert(child_id, parent_id);
    }

    /// Remove ownership tracking for a child cell.
    pub fn unmark_owned(&self, child_id: Uuid) {
        self.ownership.remove(&child_id);
    }

    /// Take a snapshot of all live cells. Automatically garbage-collects stale entries.
    pub fn snapshot(&self) -> Vec<CellSnapshot> {
        let mut snapshots = Vec::new();
        let mut stale = Vec::new();

        for entry in self.cells.iter() {
            let id = *entry.key();
            match entry.value().upgrade() {
                Some(node) => {
                    let dep_ids: Vec<Uuid> = node.deps().iter().map(|d| d.id()).collect();
                    let owner_id = self.ownership.get(&id).map(|e| *e.value());
                    let value = node.value_debug().map(|mut s| {
                        if s.len() > MAX_VALUE_LEN {
                            s.truncate(MAX_VALUE_LEN);
                            s.push('…');
                        }
                        s
                    });
                    let caller = node
                        .caller()
                        .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()));
                    snapshots.push(CellSnapshot {
                        id,
                        name: node.name(),
                        display_name: node.display_name(),
                        subscriber_count: node.subscriber_count(),
                        owned_count: node.owned_count(),
                        dep_ids,
                        owner_id,
                        value,
                        caller,
                    });
                }
                None => {
                    stale.push(id);
                }
            }
        }

        // GC stale entries
        for id in stale {
            self.cells.remove(&id);
            self.ownership.remove(&id);
        }

        snapshots
    }

    /// Remove all entries where the weak reference can no longer be upgraded.
    pub fn gc(&self) {
        let stale: Vec<Uuid> = self
            .cells
            .iter()
            .filter(|e| e.value().upgrade().is_none())
            .map(|e| *e.key())
            .collect();

        for id in stale {
            self.cells.remove(&id);
            self.ownership.remove(&id);
        }
    }

    /// Number of tracked cells (including potentially stale entries).
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

static REGISTRY: OnceLock<CellRegistry> = OnceLock::new();

/// Get the global cell registry, initializing it on first access.
pub fn registry() -> &'static CellRegistry {
    REGISTRY.get_or_init(CellRegistry::new)
}
