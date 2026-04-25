use crate::{
    CatchErrorExt, Cell, Gettable, MapErrExt, MapOkExt, Mutable, Pipeline, TryMapExt, UnwrapOrExt,
};

// ============================================================================
// TryMap
// ============================================================================

#[test]
fn test_try_map_success() {
    let source = Cell::new(10i32);
    let result = source
        .try_map(|v| -> Result<String, &str> { Ok(v.to_string()) })
        .materialize();

    assert_eq!(result.get(), Ok("10".to_string()));

    source.set(42);
    assert_eq!(result.get(), Ok("42".to_string()));
}

#[test]
fn test_try_map_failure() {
    let source = Cell::new(-5i32);
    let result = source
        .try_map(|v| -> Result<u32, &str> {
            if *v >= 0 {
                Ok(*v as u32)
            } else {
                Err("must be non-negative")
            }
        })
        .materialize();

    assert_eq!(result.get(), Err("must be non-negative"));

    source.set(10);
    assert_eq!(result.get(), Ok(10u32));

    source.set(-1);
    assert_eq!(result.get(), Err("must be non-negative"));
}

// ============================================================================
// MapOk
// ============================================================================

#[test]
fn test_map_ok_transforms_ok() {
    let source: Cell<Result<i32, &str>, _> = Cell::new(Ok(10));
    let mapped = source.map_ok(|v| v * 2);

    assert_eq!(mapped.get(), Ok(20));

    source.set(Ok(5));
    assert_eq!(mapped.get(), Ok(10));
}

#[test]
fn test_map_ok_passes_through_err() {
    let source: Cell<Result<i32, &str>, _> = Cell::new(Err("error"));
    let mapped = source.map_ok(|v| v * 2);

    assert_eq!(mapped.get(), Err("error"));

    source.set(Ok(5));
    assert_eq!(mapped.get(), Ok(10));

    source.set(Err("another error"));
    assert_eq!(mapped.get(), Err("another error"));
}

// ============================================================================
// MapErr
// ============================================================================

#[test]
fn test_map_err_transforms_err() {
    let source: Cell<Result<i32, &str>, _> = Cell::new(Err("oops"));
    let mapped = source.map_err(|e| e.len());

    assert_eq!(mapped.get(), Err(4)); // "oops".len() == 4

    source.set(Err("longer error"));
    assert_eq!(mapped.get(), Err(12));
}

#[test]
fn test_map_err_passes_through_ok() {
    let source: Cell<Result<i32, &str>, _> = Cell::new(Ok(42));
    let mapped = source.map_err(|e| e.len());

    assert_eq!(mapped.get(), Ok(42));

    source.set(Err("error"));
    assert_eq!(mapped.get(), Err(5));

    source.set(Ok(100));
    assert_eq!(mapped.get(), Ok(100));
}

// ============================================================================
// CatchError
// ============================================================================

#[test]
fn test_catch_error_recovers() {
    let source: Cell<Result<i32, &str>, _> = Cell::new(Err("failed"));
    let recovered = source.catch_error(|_| -1);

    assert_eq!(recovered.get(), -1);

    source.set(Ok(42));
    assert_eq!(recovered.get(), 42);

    source.set(Err("another failure"));
    assert_eq!(recovered.get(), -1);
}

#[test]
fn test_catch_error_with_error_info() {
    let source: Cell<Result<String, i32>, _> = Cell::new(Err(404));
    let recovered = source.catch_error(|code| format!("Error {}", code));

    assert_eq!(recovered.get(), "Error 404");

    source.set(Ok("success".to_string()));
    assert_eq!(recovered.get(), "success");
}

// ============================================================================
// UnwrapOr
// ============================================================================

#[test]
fn test_unwrap_or_with_ok() {
    let source: Cell<Result<i32, &str>, _> = Cell::new(Ok(42));
    let unwrapped = source.unwrap_or(0);

    assert_eq!(unwrapped.get(), 42);
}

#[test]
fn test_unwrap_or_with_err() {
    let source: Cell<Result<i32, &str>, _> = Cell::new(Err("error"));
    let unwrapped = source.unwrap_or(0);

    assert_eq!(unwrapped.get(), 0);

    source.set(Ok(100));
    assert_eq!(unwrapped.get(), 100);

    source.set(Err("another"));
    assert_eq!(unwrapped.get(), 0);
}

// ============================================================================
// UnwrapOrElse
// ============================================================================

#[test]
fn test_unwrap_or_else_with_ok() {
    let source: Cell<Result<i32, &str>, _> = Cell::new(Ok(42));
    let unwrapped = source.unwrap_or_else(|_| -1);

    assert_eq!(unwrapped.get(), 42);
}

#[test]
fn test_unwrap_or_else_with_err() {
    let source: Cell<Result<String, i32>, _> = Cell::new(Err(404));
    let unwrapped = source.unwrap_or_else(|code| format!("default-{}", code));

    assert_eq!(unwrapped.get(), "default-404");

    source.set(Ok("hello".to_string()));
    assert_eq!(unwrapped.get(), "hello");
}

// ============================================================================
// Chaining
// ============================================================================

#[test]
fn test_error_operator_chain() {
    let source = Cell::new(10i32);

    // try_map -> map_ok -> catch_error
    let result = source
        .try_map(|v| -> Result<i32, &str> {
            if *v > 0 { Ok(*v) } else { Err("negative") }
        })
        .materialize()
        .map_ok(|v| v * 2)
        .catch_error(|_| 0);

    assert_eq!(result.get(), 20);

    source.set(-5);
    assert_eq!(result.get(), 0);

    source.set(7);
    assert_eq!(result.get(), 14);
}
