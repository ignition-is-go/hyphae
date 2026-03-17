/// Marker trait for ID types associated with a parent/entity type `T`.
pub trait IdFor<T> {
    type MapKey;

    fn map_key(&self) -> Self::MapKey;
}

impl<T, I> IdFor<T> for Option<I>
where
    I: IdFor<T>,
{
    type MapKey = Option<I::MapKey>;

    fn map_key(&self) -> Self::MapKey {
        self.as_ref().map(IdFor::map_key)
    }
}

/// Inverse mapping from an ID type to its parent/entity type.
pub trait IdType {
    type Parent;
}

impl<I> IdType for Option<I>
where
    I: IdType,
{
    type Parent = I::Parent;
}

/// Foreign-key trait for FK joins.
///
/// Implement this on a row/value type to expose its foreign key value for parent `T`.
pub trait HasForeignKey<T> {
    type ForeignKey: IdFor<T> + IdType<Parent = T>;

    fn fk(&self) -> Self::ForeignKey;
}

/// Convert a right-side join key into the left-side join key representation.
pub trait JoinKeyFrom<R> {
    fn join_key_from(value: &R) -> Self;
}

impl<T: Clone> JoinKeyFrom<T> for T {
    fn join_key_from(value: &T) -> Self {
        value.clone()
    }
}

impl<T: Clone> JoinKeyFrom<T> for Option<T> {
    fn join_key_from(value: &T) -> Self {
        Some(value.clone())
    }
}
