#![allow(dead_code)]

//! Static type substrate for an arbitrary-length left-join region.
//!
//! A join region is described and installed without tuples (and therefore
//! without tuple-arity limits), while every stage, right-root entry, and
//! projection remains statically dispatched.
//!
//! Do not add direct deep `JCons` test instantiations here. Public composition
//! is automatically segmented into bounded typed regions, and its behavioral
//! coverage lives in the application-shaped integration tests. Instantiating
//! the legacy unbounded substrate in the monolithic lib-test crate can consume
//! tens of gigabytes during LLVM code generation.

mod declaration;
mod plan;
mod router;
mod stage_runtime;

pub use declaration::{
    CollectProject, DirectProject, JCons, JNil, JoinStage, LastStage, MapLast, OwnedIndex, Push,
    ReplaceLastProject, SharedRelationIndex, StageList, StageProject, ThenMap, foreign_map_key,
    map_key,
};
pub use plan::JoinRegion;

pub use declaration::collect_matches;
