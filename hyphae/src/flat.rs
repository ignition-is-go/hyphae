/// Macro to flatten nested tuple patterns from chained `join()` calls.
///
/// Chained joins produce left-nested tuples: `a.join(&b).join(&c)` → `((A, B), C)`.
/// This macro generates the nested pattern from a flat parameter list:
///
/// ```
/// use hyphae::{Cell, Gettable, JoinExt, MapExt, MaterializeDefinite, flat};
///
/// let a = Cell::new(1);
/// let b = Cell::new(2);
/// let c = Cell::new(3);
/// let d = Cell::new(4);
///
/// // flat!(|a, b, c, d| ...) expands to |(((a, b), c), d)| ...
/// let sum = a
///     .join(&b)
///     .join(&c)
///     .join(&d)
///     .map(flat!(|a, b, c, d| a + b + c + d))
///     .materialize();
/// assert_eq!(sum.get(), 10);
/// ```
#[macro_export]
macro_rules! flat {
    // Generate left-nested pattern for chained joins
    (|$first:ident $(, $rest:ident)+| $body:expr) => {
        |flat!(@nest ($first) $($rest),+)| $body
    };

    (@nest ($acc:pat) $next:ident $(, $rest:ident)+) => {
        flat!(@nest (($acc, $next)) $($rest),+)
    };
    (@nest ($acc:pat) $last:ident) => {
        ($acc, $last)
    };
}
