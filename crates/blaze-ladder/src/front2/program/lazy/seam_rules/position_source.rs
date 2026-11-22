//! The names source a position is found in: any [`NamesSource`]. Its three reverse lookups
//! (`member_beginning`, `enclosing_keys`, `expansion_text`) are questions of every names source
//! (front2 source.rs, questions 10i to 10k), so a body's place (`seam_rules::body_place`) is
//! asked of solved names, records and a table view alike. Before, they were this trait's own,
//! which the table view did not implement, and a body's `line!()` could not be placed by the
//! rule a `#[track_caller]` `Location` is.
//!
//! INTERIM: the name is kept so the bounds written with it stand; replace each with
//! `NamesSource` and delete it.

use super::NamesSource;

/// Every [`NamesSource`].
pub trait PositionSource<'n>: NamesSource<'n> {}

impl<'n, S: NamesSource<'n>> PositionSource<'n> for S {}
