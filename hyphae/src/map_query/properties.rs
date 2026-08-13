//! Compile-time relational properties for sealed map-query plan nodes.

use std::{hash::Hash, marker::PhantomData};

use crate::traits::CellValue;

pub trait Cardinality: Send + Sync + 'static {}
pub struct ExactlyOne;
pub struct ZeroOrOne;
pub struct Many;

impl Cardinality for ExactlyOne {}
impl Cardinality for ZeroOrOne {}
impl Cardinality for Many {}

pub trait Partition: Send + Sync + 'static {}

/// Rows remain partitioned by their map key.
pub struct ByMapKey<K>(PhantomData<fn() -> K>);
/// Rows are physically partitioned by a typed relationship.
pub struct ByRelation<Rel>(PhantomData<fn() -> Rel>);
/// The operator changes keys and therefore establishes a new partition.
pub struct Repartition<K>(PhantomData<fn() -> K>);

impl<K: Send + Sync + 'static> Partition for ByMapKey<K> {}
impl<Rel: Send + Sync + 'static> Partition for ByRelation<Rel> {}
impl<K: Send + Sync + 'static> Partition for Repartition<K> {}

/// Static facts used to select legal physical-plan composition.
pub trait PlanProperties {
    type Cardinality: Cardinality;
    type InputPartition: Partition;
    type OutputPartition: Partition;
}

/// Proof accepted by key-preserving fused regions.
pub trait PreservesMapKey<K>: PlanProperties<OutputPartition = ByMapKey<K>>
where
    K: Hash + Eq + CellValue,
{
}

impl<K, P> PreservesMapKey<K> for P
where
    K: Hash + Eq + CellValue,
    P: PlanProperties<OutputPartition = ByMapKey<K>>,
{
}
