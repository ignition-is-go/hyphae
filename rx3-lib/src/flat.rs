/// Macro to flatten nested tuple patterns from chained `join()` calls.
///
/// When you chain multiple `join()` calls, you get nested tuples like `((a, b), c)`.
/// This macro lets you write flat parameter lists instead:
///
/// ```
/// use rx3::{Cell, Gettable, JoinExt, MapExt, flat};
///
/// let a = Cell::new(1);
/// let b = Cell::new(2);
/// let c = Cell::new(3);
///
/// // Without flat! - nested tuple destructuring
/// let sum = a.join(&b).join(&c).map(|((x, y), z)| x + y + z);
///
/// // With flat! - clean parameter list
/// let sum = a.join(&b).join(&c).map(flat!(|x, y, z| x + y + z));
///
/// assert_eq!(sum.get(), 6);
/// ```
#[macro_export]
macro_rules! flat {
    // 2 params: (a, b)
    (|$a:ident, $b:ident| $body:expr) => {
        |($a, $b)| $body
    };
    // 3 params: ((a, b), c)
    (|$a:ident, $b:ident, $c:ident| $body:expr) => {
        |(($a, $b), $c)| $body
    };
    // 4 params: (((a, b), c), d)
    (|$a:ident, $b:ident, $c:ident, $d:ident| $body:expr) => {
        |((($a, $b), $c), $d)| $body
    };
    // 5 params: ((((a, b), c), d), e)
    (|$a:ident, $b:ident, $c:ident, $d:ident, $e:ident| $body:expr) => {
        |(((($a, $b), $c), $d), $e)| $body
    };
    // 6 params
    (|$a:ident, $b:ident, $c:ident, $d:ident, $e:ident, $f:ident| $body:expr) => {
        |((((($a, $b), $c), $d), $e), $f)| $body
    };
}
