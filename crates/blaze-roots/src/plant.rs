//! The bugs a control can plant in [`crate::ReadList::under_planted`] (D165, rule 78): shipped
//! code passes `NoPlant` (through `under`), so every check compiles to `false`.

#![blaze::sans_io]

use blaze_own::planted::Plants;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RootsPlant {
    /// The check as it is.
    None,
    /// A package root's globs are not consulted: an undeclared file passes.
    GlobsIgnored,
    /// A read under the output root passes as an input.
    OutputReadable,
    /// A path seen two ways keeps its first sight instead of refusing the list.
    MovedKept,
}

impl Plants for RootsPlant {}
