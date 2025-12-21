/// Macro to create tuple-destructuring closures for `join()` results.
///
/// Since closures take a single argument, joined cells require tuple patterns.
/// This macro lets you write comma-separated patterns that mirror your join structure:
///
/// ```
/// use rx3::{Cell, Gettable, JoinExt, MapExt, flat};
///
/// let a = Cell::new(1);
/// let b = Cell::new(2);
/// let c = Cell::new(3);
///
/// // Two cells: a.join(&b) produces (A, B)
/// let sum = a.join(&b).map(flat!(|x, y| x + y));
/// assert_eq!(sum.get(), 3);
///
/// // Chain: a.join(&b).join(&c) produces ((A, B), C)
/// let sum = a.join(&b).join(&c).map(flat!(|(x, y), z| x + y + z));
/// assert_eq!(sum.get(), 6);
/// ```
///
/// For tree-shaped joins, mirror the structure:
///
/// ```
/// use rx3::{Cell, Gettable, JoinExt, MapExt, flat};
///
/// let a = Cell::new(1);
/// let b = Cell::new(2);
/// let c = Cell::new(3);
/// let d = Cell::new(4);
///
/// // ab.join(&cd) produces ((A, B), (C, D))
/// let ab = a.join(&b);
/// let cd = c.join(&d);
/// let sum = ab.join(&cd).map(flat!(|(a, b), (c, d)| a + b + c + d));
/// assert_eq!(sum.get(), 10);
/// ```
#[macro_export]
macro_rules! flat {
    // Wrap comma-separated patterns in a tuple
    (|$($pat:pat_param),+| $body:expr) => {
        |($($pat),+)| $body
    };
}
