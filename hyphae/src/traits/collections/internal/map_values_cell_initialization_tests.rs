use std::sync::Arc;

use crate::{
    Cell, CellMap, DepNode, Gettable, MapDiff, MapQuery, Materialize, Mutable, ProjectCellExt,
    Signal, SubscriptionGuard, Watchable, cell::CellMutable,
};

#[derive(Clone)]
struct UpdateBeforeSubscribe {
    value: Cell<Option<(u8, u8)>, CellMutable>,
}

impl Gettable<Option<(u8, u8)>> for UpdateBeforeSubscribe {
    fn get(&self) -> Option<(u8, u8)> {
        self.value.get()
    }
}

impl DepNode for UpdateBeforeSubscribe {
    fn id(&self) -> uuid::Uuid {
        self.value.id()
    }

    fn name(&self) -> Option<String> {
        self.value.name()
    }

    fn deps(&self) -> Vec<Arc<dyn DepNode>> {
        self.value.deps()
    }
}

impl Watchable<Option<(u8, u8)>> for UpdateBeforeSubscribe {
    fn subscribe(
        &self,
        callback: impl Fn(&Signal<Option<(u8, u8)>>) + Send + Sync + 'static,
    ) -> SubscriptionGuard {
        self.value.set(Some((7, 42)));
        self.value.subscribe(callback)
    }

    fn unsubscribe(&self, id: uuid::Uuid) {
        self.value.unsubscribe(id);
    }

    fn is_complete(&self) -> bool {
        self.value.is_complete()
    }

    fn is_error(&self) -> bool {
        self.value.is_error()
    }

    fn error(&self) -> Option<Arc<anyhow::Error>> {
        self.value.error()
    }
}

#[test]
fn projection_initialization_observes_change_before_subscription_replay() {
    let source = CellMap::<u8, ()>::new();
    source.insert(1, ());
    let value = Cell::new(None);
    let output = source
        .project_cell({
            let value = value.clone();
            move |_, ()| UpdateBeforeSubscribe {
                value: value.clone(),
            }
        })
        .materialize();

    assert_eq!(value.get(), Some((7, 42)));
    assert_eq!(output.get(&7).materialize().get(), Some(42));
}

#[test]
fn projection_initialization_batch_keeps_newer_inner_value() {
    let source = CellMap::<u8, ()>::new();
    let first = Cell::new(None);
    let second = Cell::new(Some((8, 84)));
    let output = source
        .clone()
        .project_cell({
            let first = first.clone();
            move |key, ()| {
                if *key == 1 {
                    first.clone()
                } else {
                    first.set(Some((7, 42)));
                    second.clone()
                }
            }
        })
        .materialize();

    source.insert_many(vec![(1, ()), (2, ())]);

    assert_eq!(first.get(), Some((7, 42)));
    assert_eq!(output.get(&8).materialize().get(), Some(84));
    assert_eq!(output.get(&7).materialize().get(), Some(42));
}

#[test]
fn projection_initialization_reentrant_sink_keeps_latest_value() {
    let source = CellMap::<u8, ()>::new();
    let value = Cell::new(Some((7, 10)));
    let output = source
        .clone()
        .project_cell({
            let value = value.clone();
            move |_, ()| value.clone()
        })
        .materialize();
    let _guard = output.subscribe_diffs({
        let value = value.clone();
        move |diff| {
            if matches!(diff, MapDiff::Insert { key: 7, value: 10 }) {
                value.set(Some((7, 20)));
            }
        }
    });

    source.insert(1, ());

    assert_eq!(value.get(), Some((7, 20)));
    assert_eq!(output.get(&7).materialize().get(), Some(20));
}

#[test]
fn projection_initialization_ignores_old_callback_after_remove_or_reattach() {
    for replace in [false, true] {
        let source = CellMap::<u8, u8>::new();
        source.insert(1, 0);
        let old_value = Cell::new(Some((7, 10)));
        let new_value = Cell::new(Some((7, 20)));
        let _old_guard = old_value.subscribe({
            let source = source.clone();
            move |signal| {
                if matches!(signal, Signal::Value(value) if **value == Some((7, 30))) {
                    if replace {
                        source.insert(1, 1);
                    } else {
                        source.remove(&1);
                    }
                }
            }
        });
        let output = source
            .project_cell({
                let old_value = old_value.clone();
                move |_, version| {
                    if *version == 0 {
                        old_value.clone()
                    } else {
                        new_value.clone()
                    }
                }
            })
            .materialize();

        old_value.set(Some((7, 30)));

        assert_eq!(
            output.get(&7).materialize().get(),
            if replace { Some(20) } else { None }
        );
    }
}

#[test]
fn projection_initialization_queued_old_replay_cannot_replace_new_subscription() {
    let source = CellMap::<u8, u8>::new();
    let old_value = Cell::new(Some((7, 10)));
    let new_value = Cell::new(Some((7, 20)));
    let output = source
        .clone()
        .project_cell({
            let old_value = old_value.clone();
            move |_, version| {
                if *version == 0 {
                    old_value.clone()
                } else {
                    new_value.clone()
                }
            }
        })
        .materialize();
    let _guard = output.subscribe_diffs({
        let source = source.clone();
        move |diff| {
            if matches!(diff, MapDiff::Insert { key: 7, value: 10 }) {
                source.insert(1, 1);
                old_value.set(Some((7, 30)));
            }
        }
    });

    source.insert(1, 0);

    assert_eq!(output.get(&7).materialize().get(), Some(20));
}

#[test]
fn projection_initialization_drop_releases_inner_subscription_and_mapper() {
    let source = CellMap::<u8, ()>::new();
    source.insert(1, ());
    let value = Cell::new(Some((7, 42)));
    let captured = Arc::new(value.clone());
    let weak = Arc::downgrade(&captured);
    let output = source
        .clone()
        .project_cell(move |_, ()| captured.as_ref().clone())
        .materialize();

    assert_eq!(output.get(&7).materialize().get(), Some(42));
    assert!(weak.upgrade().is_some());
    drop(output);

    assert!(weak.upgrade().is_none());
    assert_eq!(value.subscriber_count(), 0);
    assert_eq!(source.snapshot(), vec![(1, ())]);
    assert_eq!(value.get(), Some((7, 42)));
}
