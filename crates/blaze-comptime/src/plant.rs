//! The bugs a control can plant in the host machine (D165, rule 78). Shipped code passes
//! `NoPlant`, so every check compiles to `false`.

#![blaze::sans_io]

use blaze_own::planted::Plants;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostPlant {
    /// The machine as it is.
    None,
    /// A path a package's globs do not declare is looked up and answered with its content.
    OutsideAnswered,
    /// An answered lookup is left out of the read list.
    ReadUnlisted,
    /// A clock read is answered with the host's time instead of the grants' fixed clock.
    HostClock,
}

impl Plants for HostPlant {}
