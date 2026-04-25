//! Integration tests for Pipeline type.

use crate::{Cell, CellMutable, Gettable, MapExt, Mutable, Pipeline};

#[test]
fn cell_is_pipeline() {
    let c = Cell::new(10);
    // Cell: Pipeline<i32> — get() comes from Gettable, which Pipeline requires.
    // Force the Pipeline bound to be the resolved path so the test fails to
    // compile if Cell ever stops implementing Pipeline.
    fn assert_pipeline<T: crate::traits::CellValue, P: Pipeline<T>>(p: &P) -> T {
        p.get()
    }
    let v: i32 = assert_pipeline::<i32, Cell<i32, CellMutable>>(&c);
    assert_eq!(v, 10);
}

#[test]
fn pipeline_materialize_roundtrip() {
    let c = Cell::new(5).with_name("src");
    // Pipeline::materialize on a plain Cell returns a Cell with the same value
    let mat = c.clone().materialize();
    assert_eq!(mat.get(), 5);

    c.set(99);
    assert_eq!(mat.get(), 99);
}

#[test]
fn map_pipeline_does_not_allocate_intermediate_cell() {
    let src = Cell::new(5).with_name("src");
    let pipeline = src.clone().map(|x| x * 2);
    // The pipeline is NOT a Cell — Pipeline::get() recomputes from source.
    let v: i32 = pipeline.get();
    assert_eq!(v, 10);
    src.set(7);
    assert_eq!(pipeline.get(), 14);
}

#[test]
fn map_pipeline_materializes_to_subscribable_cell() {
    let src = Cell::new(3);
    let doubled = src.clone().map(|x| x * 2).materialize();
    assert_eq!(doubled.get(), 6);
    src.set(10);
    assert_eq!(doubled.get(), 20);
}

#[test]
fn map_pipeline_chains_fuse_into_one_subscription() {
    use crate::traits::DepNode;

    let src = Cell::new(1).with_name("src");
    let initial_count = DepNode::subscriber_count(&src);

    let mat = src
        .clone()
        .map(|x| x + 1)
        .map(|x| x * 2)
        .map(|x| x + 10)
        .materialize();

    assert_eq!(
        DepNode::subscriber_count(&src),
        initial_count + 1,
        "chained pipeline must install exactly one subscription on root"
    );
    assert_eq!(mat.get(), 14);
    src.set(5);
    assert_eq!(mat.get(), 22);
}

use crate::FilterExt;

#[test]
fn filter_pipeline_passes_matching_and_blocks_non_matching() {
    let src = Cell::new(10u64);
    let evens = src.filter(|x| x % 2 == 0).materialize();

    assert_eq!(evens.get(), 10);
    src.set(3);
    assert_eq!(evens.get(), 10);
    src.set(6);
    assert_eq!(evens.get(), 6);
}

#[test]
fn filter_pipeline_fuses_with_map() {
    use crate::traits::DepNode;

    let src = Cell::new(1i64).with_name("src");
    let initial_count = DepNode::subscriber_count(&src);

    let out = src
        .map(|x| x + 10)
        .filter(|x| x % 2 == 0)
        .map(|x| x * 100)
        .materialize();

    assert_eq!(DepNode::subscriber_count(&src), initial_count + 1);
    src.set(2); // 2+10=12, even, passes
    assert_eq!(out.get(), 1200);
}
