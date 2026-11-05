//! blaze v2's dependency image: every dependency function and static of a dev build
//! prelinked once into one image at a fixed base, keyed by the dependency graph, kept in the
//! store and shared by every view and run of the machine, so a dev link places only the
//! workspace's own code and resolves the rest against the image's table.
//!
//! - [`key`]: what an image is made from, hashed ([`GraphKey`], [`ImageKey`]).
//! - [`split`]: which pieces of a build are the image's and which the workspace link's.
//! - [`prelink`]: the image linked by blaze-link at its base, checked ([`Prelink`]).
//! - [`table`], [`image`]: the image's format: its segments, symbol table by instance key,
//!   open cells, libraries, init and fini pieces, thread-local template.
//! - [`map`]: the image as one memory object every run maps at its linked address, inside the
//!   workspace executable's reservation for fixed maps.
//! - [`start`]: the image's open cells, bound as a whole link binds them; the rest of a run's
//!   start is the system loader's, on the executable.
//! - [`cache`], [`store`], [`dev`]: one image per key on the machine, made again only when
//!   the key changes, kept in the store.
//!
//! Design and the case for each behavior: `docs/v2/plan/lanes/fastcompile/premap.md`.

#![forbid(unsafe_code)]
#![feature(allocator_api, allocator_ext)]

use std::fmt;

pub mod cache;
pub mod dev;
pub mod image;
pub mod key;
pub mod map;
pub mod prelink;
pub mod split;
pub mod start;
pub mod store;
pub mod table;

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;

pub use cache::{Images, Pick, Ready};
pub use dev::{Dev, Job, Plan, plan};
pub use image::View;
pub use key::{GraphInputs, GraphKey, ImageInputs, ImageKey};
pub use prelink::{Placed, Prelink, Prelinked, Request};
pub use split::{Built, Fit, Owned, SidedNative, Stale, WorkspacePart};
pub use store::Shelf;
pub use table::{Def, SymbolId, TableView};

/// What premap could not do, and where.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PremapError {
    pub what: String,
}

impl PremapError {
    pub fn new(what: String) -> PremapError {
        PremapError { what }
    }

    /// The error `what` says, its message made here, once, when premap fails.
    pub fn of(what: fmt::Arguments<'_>) -> PremapError {
        PremapError { what: fmt::format(what) } // alloc: error: its message
    }
}

impl fmt::Display for PremapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dependency image: {}", self.what)
    }
}

impl std::error::Error for PremapError {}

impl From<blaze_inproc::Error> for PremapError {
    fn from(error: blaze_inproc::Error) -> PremapError {
        PremapError::of(format_args!("{error}"))
    }
}
