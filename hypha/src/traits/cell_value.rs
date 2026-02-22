use std::fmt::Debug;

/// Supertrait combining all bounds required for cell value types.
///
/// Any type used as a cell value must satisfy
/// `Clone + Debug + PartialEq + Send + Sync + 'static`.
/// This trait is automatically implemented for all qualifying types via a blanket impl.
pub trait CellValue: Clone + Debug + PartialEq + Send + Sync + 'static {}

impl<T: Clone + Debug + PartialEq + Send + Sync + 'static> CellValue for T {}
