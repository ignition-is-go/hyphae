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

/// A named foreign-key relationship between a child row and a parent keyspace.
///
/// The zero-sized implementor is the relationship's identity, allowing a
/// child type to declare multiple relationships to the same parent type. Query
/// plans use this type identity to monomorphize access and, later, to share a
/// single physical relationship index across repeated joins.
pub trait ForeignKeyRelation: Send + Sync + 'static {
    /// Semantic parent entity type.
    type Parent: Send + Sync + 'static;
    /// Child row type containing the foreign key.
    type Child: super::CellValue;
    /// Typed identifier for `Parent`.
    type ForeignKey: IdFor<Self::Parent> + super::CellValue;

    /// Extract the foreign key, or `None` for an absent optional relationship.
    fn foreign_key(child: &Self::Child) -> Option<Self::ForeignKey>;
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
