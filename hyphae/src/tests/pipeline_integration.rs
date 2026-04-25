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

use crate::{FilterExt, TryMapExt};

#[test]
fn filter_pipeline_passes_matching_and_blocks_non_matching() {
    let src = Cell::new(10u64);
    let evens = src.clone().filter(|x| x % 2 == 0).materialize();

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
        .clone()
        .map(|x| x + 10)
        .filter(|x| x % 2 == 0)
        .map(|x| x * 100)
        .materialize();

    assert_eq!(DepNode::subscriber_count(&src), initial_count + 1);
    src.set(2); // 2+10=12, even, passes
    assert_eq!(out.get(), 1200);
}

#[test]
fn try_map_pipeline_produces_result_cell() {
    let src = Cell::new(10i32);
    let parsed = src
        .clone()
        .try_map(|v| if *v > 0 { Ok(v.to_string()) } else { Err("must be positive") })
        .materialize();

    assert_eq!(parsed.get(), Ok("10".to_string()));
    src.set(-5);
    assert_eq!(parsed.get(), Err("must be positive"));
}

use std::sync::Arc;

use crate::TapExt;

#[test]
fn tap_pipeline_observes_without_modifying() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let src = Cell::new(0u64);
    let seen = Arc::new(AtomicU64::new(0));

    let seen_clone = Arc::clone(&seen);
    let mat = src
        .clone()
        .tap(move |v| {
            seen_clone.store(*v, Ordering::SeqCst);
        })
        .materialize();

    src.set(42);
    assert_eq!(seen.load(Ordering::SeqCst), 42);
    assert_eq!(mat.get(), 42);
}

use crate::{CatchErrorExt, MapErrExt, MapOkExt, UnwrapOrExt};

#[test]
fn map_ok_transforms_only_ok() {
    let src: Cell<Result<i32, String>, _> = Cell::new(Ok(5));
    let doubled = src.clone().map_ok(|v| v * 2).materialize();

    assert_eq!(doubled.get(), Ok(10));
    src.set(Err("boom".to_string()));
    assert_eq!(doubled.get(), Err("boom".to_string()));
}

#[test]
fn map_err_transforms_only_err() {
    let src: Cell<Result<i32, String>, _> = Cell::new(Err("oops".to_string()));
    let wrapped = src.clone().map_err(|e| format!("wrapped: {}", e)).materialize();

    assert_eq!(wrapped.get(), Err("wrapped: oops".to_string()));
    src.set(Ok(99));
    assert_eq!(wrapped.get(), Ok(99));
}

#[test]
fn catch_error_recovers() {
    let src: Cell<Result<i32, String>, _> = Cell::new(Err("bad".to_string()));
    let recovered = src.clone().catch_error(|_| 0i32).materialize();

    assert_eq!(recovered.get(), 0);
    src.set(Ok(42));
    assert_eq!(recovered.get(), 42);
}

#[test]
fn unwrap_or_provides_default() {
    let src: Cell<Result<i32, String>, _> = Cell::new(Err("bad".to_string()));
    let unwrapped = src.clone().unwrap_or(-1i32).materialize();

    assert_eq!(unwrapped.get(), -1);
    src.set(Ok(77));
    assert_eq!(unwrapped.get(), 77);
}
