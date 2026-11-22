//! The bugs a control can plant in the map (rule 53; D165, "a control is typed"): shipped code
//! takes one through [`Planted<DataPlant>`](blaze_own::planted::Planted), and every caller but
//! the map's control passes [`NoPlant`](blaze_own::planted::NoPlant), which compiles each
//! `has` to `false`.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// A bug a control plants in [`Map`](crate::Map).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DataPlant {
    /// The map as it is.
    None,
    /// Inserting a present key adds a second entry instead of replacing the first.
    DuplicateOnHit,
}

impl Plants for DataPlant {}
