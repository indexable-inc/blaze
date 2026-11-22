//! The note an E0425 carries when the path began at `std` and `std` is bstd (rule 19).
//!
//! Under a blaze edition `std` names bstd and Rust's `std` has no name at all (blaze-unit-graph
//! implicit.rs, the rule's one home: "blaze edition (lib, bin, test) | .. | bstd; Rust's `std`
//! none", and "There is no opt-in to Rust's `std` under the edition"). A path written for Rust's
//! `std` (`std::sync::OnceLock::new()`) then fails at a later segment with nothing in the finding
//! saying that `std` was not the `std` its writer meant: a writer lost an hour to it on
//! 2026-11-16. So when the miss is after a first segment `std` that is the extern prelude's
//! `std` of a crate whose `std` is bstd, the prefix the finding names is
//! [`tc::PathPrefix::Bstd`], carrying the edition's name, and the finding's words say it
//! (path_miss_shown.rs).
//!
//! `core` and `alloc` are Rust's under every edition (the same table), so only `std` is noted.

use blaze_id::ItemKey as LibKey;
use blaze_scope::names::{PathError, Unheld, Why};
use blaze_typeck::{self as tc, Fetch};
use blaze_unit_graph::StdIs;

use super::source::NamesSource;
use super::world_items::{BodyWorld, Gap};
use crate::resolve::{Ns, ResolveError};

/// The written first segment the note is about: the extern prelude's name of a unit's `std`.
const STD: &str = "std";

impl<'n, S: NamesSource<'n>> BodyWorld<'_, 'n, S> {
    /// The blaze edition's name (`blaze-2026.11`) when the miss `why` of `rest` (read at
    /// `module`, with no `use` or glob in front: `shift == 0`) is after a first segment `std`
    /// that names bstd: the body's crate's `std` is bstd ([`NamesSource::std_is`]) and the
    /// written `std` is that crate's extern prelude `std` (not a module or item of the program
    /// named `std`, which a plain path finds first). `None`: no note.
    pub(super) fn std_is_bstd(&self, module: LibKey, rest: &[&str], shift: u32, why: tc::MissWhy) -> Fetch<Option<&'static str>> {
        // Only a segment not found after the written `std`: a private item or an ambiguity is
        // found, and its own words say so.
        if shift != 0 || why != tc::MissWhy::NotFound || rest.len() < 2 || rest[0] != STD {
            return Ok(None);
        }
        let not_read = || Gap::Resolve(ResolveError::Path(PathError { module, at: 0, why: Why::NotRead { module, layer: Unheld::Step } }));
        let Some(krate) = self.names.crate_of(module) else { return self.gap(not_read()) };
        let edition = match self.names.std_is(krate) {
            Some(StdIs::Rust) => return Ok(None),
            Some(StdIs::Bstd(edition)) => edition,
            None => return self.gap(not_read()),
        };
        // The written `std` as a plain path, and the extern prelude's `std` (`::std`, the
        // Reference, "Paths", "Path qualifiers", `::`: "in the 2018 edition or later, paths
        // starting with `::` resolve to a crate in the extern prelude"; every blaze edition is
        // 2018 or later). Both resolved on this error's path only.
        let written = match self.resolver().path(module, false, &rest[..1], Ns::Type) {
            Ok(t) => t,
            Err(e) => return self.gap(Gap::Resolve(e)),
        };
        let prelude = match self.resolver().path(module, true, &rest[..1], Ns::Type) {
            Ok(t) => t,
            Err(e) => return self.gap(Gap::Resolve(e)),
        };
        Ok((written.key == prelude.key).then_some(edition.manifest()))
    }
}
