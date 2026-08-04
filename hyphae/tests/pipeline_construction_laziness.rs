use std::time::Duration;

use hyphae::{
    Cell, DelayExt, DepNode, JoinExt, MapExt, MaterializeDefinite, ScanExt, SwitchMapExt, ZipExt,
};

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
        .delay(Duration::from_secs(60))
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
