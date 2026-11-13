//! The bottom of the front end as kernel steps (docs/v2/plan/design/end-state.md, "The front
//! end as kernel steps", the review's points 3, 4 and 7, piece 2 of its order).
//!
//! - [`items::file_items`] is `items(file)`: a pure function of a file's tree giving a table,
//!   one [`entry::Entry`] an item under a stable, content-hashed [`entry::ItemId`], with the
//!   hash of the item's header over tokens (comments, whitespace and positions left out), the
//!   attributes that gate or transform it, and the items taken out of it; and, as a part of
//!   its own ([`entry::body_key`]), the body's syntax record ([`entry::Body`]): its tokens,
//!   which is what reads a body instead of the file's tree. Positions are a separate part ([`lines`]), so a comment edit changes the line part
//!   and no entry.
//! - [`parse::parse`] is `parse(file, edition)`: the tree as the record the tree crate stores.
//! - [`modules::modules`] is `modules(crate)`: the crate's file set, reading the existence of
//!   both candidates of every declared module.
//! - [`steps`] makes each a kernel step over a world of files and cfg.
//! - [`table`] names the kernel's table (`blaze_kernel::table`), which every value here is:
//!   another step reads one part (an item's entry, its body, a line row, a module).
//!
//! The bar (AGENTS.md rule 67): a comment edit costs the floor's reparse of the file plus one
//! hash pass and changes 0 entries; a body edit changes that item's entry alone; inserting an
//! item changes no other item's id.
//!
//! The crate is on `core` and `alloc` (the interim form of the bstd rule while cargo builds
//! blaze), does no IO and holds no clock.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
// `#![blaze::sans_io]` (on each module) marks a sans-IO core for `blaze::sans_io_core`; inert
// under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

extern crate alloc;

pub mod entry;
pub mod items;
pub mod lines;
pub mod modules;
#[cfg(test)]
mod cutoffs;
#[cfg(test)]
mod oracle;
pub mod parse;
pub mod steps;
pub mod table;
#[cfg(test)]
mod tests;

pub use entry::{Entry, Gate, ItemId, Kind};
pub use items::{FileItems, ITEMS, file_items};
pub use parse::{Reading, parse, tree};
pub use table::{Family, Table, TableError};
