//! The bugs a control can plant in the pack reader and the tree walk (D165, rule 78: a strategy
//! is a type parameter at one seam). [`PlantedIndex`](crate::PlantedIndex) holds one as its
//! type parameter `P: Planted<GitFetchPlant>` and the tree walk asks the index's; shipped code
//! uses [`Index`](crate::Index), the index with `NoPlant`, whose every `has` is `false`, so each
//! check compiles away. The controls (tests.rs) pass a value.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// A bug a control plants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GitFetchPlant {
    /// The code as it is.
    None,
    /// A pack's trailer is not checked.
    TrailerUnchecked,
    /// A delta's copy from a nonzero offset starts one byte late.
    CopyOffsetOffByOne,
    /// A tree entry named `..` is taken.
    ParentNameTaken,
    /// A lookup by id that misses takes the entry where the id would be (an object accepted
    /// without its id matching).
    NearestId,
}

impl Plants for GitFetchPlant {}
