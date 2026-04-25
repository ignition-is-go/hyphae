//! Integration tests for Pipeline type.

use crate::{Cell, CellMutable, Gettable, Mutable, Pipeline};

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
