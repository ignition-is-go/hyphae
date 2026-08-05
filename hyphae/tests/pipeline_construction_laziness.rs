use std::time::Duration;

use hyphae::{
    Cell, CellMap, CellSet, DelayExt, DepNode, Gettable, JoinExt, MapExt, Materialize, Mutable,
    SampleOnSourceExt, ScanExt, Source, SwitchMapExt, ZipExt,
};

#[test]
fn derived_collection_views_compose_before_materialization() {
    let map = CellMap::<String, i32>::new();
    let key = "answer".to_string();
    let mapped = map
        .get(&key)
        .map(|value| value.unwrap_or_default() * 2)
        .materialize();

    assert_eq!(mapped.get(), 0);
    map.insert(key, 21);
    assert_eq!(mapped.get(), 42);

    let set = CellSet::<String>::new();
    let member = "present".to_string();
    let described = set
        .contains(&member)
        .map(|present| if *present { "yes" } else { "no" })
        .materialize();

    assert_eq!(described.get(), "no");
    set.insert(member);
    assert_eq!(described.get(), "yes");
}

#[test]
fn materialized_internal_view_still_exposes_only_a_pipeline_contract() {
    let value = Cell::new(7i32);
    let notifier = Source::<()>::new();
    let sampled = value
        .sample_on(&notifier)
        .map(|value| value * 3)
        .materialize();

    assert_eq!(sampled.get(), 21);
    value.set(9);
    notifier.emit(());
    assert_eq!(sampled.get(), 27);
}

#[test]
fn opaque_view_pipelines_do_not_borrow_their_source_handles() {
    let entries = {
        let map = CellMap::<String, i32>::new();
        map.insert("answer".to_string(), 42);
        map.entries()
    };
    assert_eq!(entries.materialize().get().len(), 1);

    let sampled = {
        let value = Cell::new(7i32);
        let notifier = Source::<()>::new();
        value.sample_on(&notifier).map(|value| value * 2)
    };
    assert_eq!(sampled.materialize().get(), 14);
}

#[test]
fn stateful_pipeline_subscribes_only_when_materialized() {
    let source = Cell::new(1i32);
    let pipeline = source
        .clone()
        .scan(0, |acc, value| acc + value)
        .map(|value| value * 2);

    assert_eq!(source.subscriber_count(), 0);

    let output = pipeline.materialize();
    assert_eq!(source.subscriber_count(), 1);

    drop(output);
    assert_eq!(source.subscriber_count(), 0);
}

#[test]
fn multi_source_pipelines_subscribe_only_when_materialized() {
    let join_left = Cell::new(1i32);
    let join_right = Cell::new(2i32);
    let zip_left = Cell::new(3i32);
    let zip_right = Cell::new(4i32);

    let joined = join_left
        .clone()
        .join(join_right.clone())
        .map(|pair| pair.0 + pair.1);
    let zipped = zip_left
        .clone()
        .zip(zip_right.clone())
        .map(|pair| pair.0 + pair.1);

    for root in [&join_left, &join_right, &zip_left, &zip_right] {
        assert_eq!(root.subscriber_count(), 0);
    }

    let joined_output = joined.materialize();
    assert_eq!(join_left.subscriber_count(), 1);
    assert_eq!(join_right.subscriber_count(), 1);
    assert_eq!(zip_left.subscriber_count(), 0);
    assert_eq!(zip_right.subscriber_count(), 0);

    let zipped_output = zipped.materialize();
    assert_eq!(zip_left.subscriber_count(), 1);
    assert_eq!(zip_right.subscriber_count(), 1);

    drop((joined_output, zipped_output));
    for root in [&join_left, &join_right, &zip_left, &zip_right] {
        assert_eq!(root.subscriber_count(), 0);
    }
}

#[test]
fn timer_pipeline_subscribes_only_when_materialized() {
    let source = Cell::new(1i32);
    let pipeline = source
        .clone()
        .delay(Duration::from_mins(1))
        .map(|value| value + 1);

    assert_eq!(source.subscriber_count(), 0);

    let output = pipeline.materialize();
    assert_eq!(source.subscriber_count(), 1);

    drop(output);
    assert_eq!(source.subscriber_count(), 0);
}

#[test]
fn dynamic_source_pipeline_subscribes_only_when_materialized() {
    let outer = Cell::new(0usize);
    let inner = Cell::new(7i32);
    let selected_inner = inner.clone();
    let pipeline = outer
        .clone()
        .switch_map(move |_| selected_inner.clone())
        .map(|value| value + 1);

    assert_eq!(outer.subscriber_count(), 0);
    assert_eq!(inner.subscriber_count(), 0);

    let output = pipeline.materialize();
    assert_eq!(outer.subscriber_count(), 1);
    assert_eq!(inner.subscriber_count(), 1);

    drop(output);
    assert_eq!(outer.subscriber_count(), 0);
    assert_eq!(inner.subscriber_count(), 0);
}
