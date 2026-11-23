//! WHAT A RUN MAY SEE, AND WHAT IT SAW: the one home (rule 30) of the roots a hermetic or
//! sandboxed run is given and of the list of what it read, for every such run blaze makes: a
//! compile-time run (a proc macro or build script on blaze's own code, crates/blaze-comptime,
//! D175), the pinned C compiler run as blaze's hermetic tool (D174, docs/v2/plan/design/c.md
//! part 1), and sandboxed tests.
//!
//! - [`Roots`]: a small set of roots (at most [`MOST_ROOTS`]), each a [`RootKind`]: a
//!   digest-keyed immutable tree from the store or the shelf, a package directory with its
//!   declared globs, or the one output directory (the only root a run may write).
//! - [`RelPath`]: a path relative to one root that cannot name anything outside it. Its
//!   constructors are the whole of the review rule "a path escaping its directory".
//! - [`Mounts`]: how the spelling a run uses (an absolute path such as the value of
//!   `CARGO_MANIFEST_DIR` it was given, or a path the operating system reported) maps to a root
//!   and a [`RelPath`]. A mount spelling is never in a key or a record (rule 71's spirit for
//!   paths: the record holds the root and the relative path).
//! - [`ReadList`]: every file read with its content digest, every lookup that FAILED, every
//!   directory listed with the digest of its names; and [`ReadList::under`], the one check,
//!   which returns the proof [`Checked`] (rule 74) that the code making a record takes.
//!
//! # Paths are checked UTF-8
//!
//! A [`RelPath`] is text, not bytes: a record and a key hold one spelling of a path on every
//! host (rule 25), a finding prints it as written, and a package's files are UTF-8 by the
//! registry's rule (cargo refuses to package a non-UTF-8 name). A name that is not UTF-8 is a
//! typed refusal ([`PathError::NotUtf8`]) at the place the bytes arrive, never a lossy decode.
//! The canonical form is one spelling per path: no leading `/`, no trailing `/`, no empty
//! segment, no `.` or `..` segment, no NUL. The empty path is the root itself.
//!
//! # A failed lookup is an input
//!
//! A run that asked for `build/config.h` and was told "no such file" took a different branch
//! than it would have with the file there: the file appearing later moves an input. So a failed
//! lookup is an entry of its own ([`Seen::Absent`]) keyed like a read, by (root, path). A record
//! made from the run stays valid while each such path is still absent; under a
//! [`RootKind::Tree`] it can never appear (the tree is immutable under its digest), under a
//! [`RootKind::Package`] the next check of the record looks the path up again. A lookup the
//! globs do not admit is not an absence but a refusal ([`Outside::NotDeclared`]): the run asked
//! for what it was not granted.
//!
//! # Lexical resolution and links
//!
//! [`Mounts::resolve`] resolves `.` and `..` by the text alone. A symbolic link inside a root is
//! the driver's: it opens beneath the root (`openat2` with `RESOLVE_BENEATH` on Linux, its
//! equal on macOS, or the tree's own entries for a digest-keyed tree), so a link that leaves the
//! root fails there, and the driver reports the read under the path the run asked for.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

mod glob;
mod list;
mod mounts;
mod path;
mod plant;
mod roots;
pub mod seatbelt;

#[cfg(test)]
mod tests;

pub use glob::{Glob, GlobError};
pub use list::{Checked, Entry, Full, Outside, ReadList, ReadListOpts, Refused, Seen};
pub use mounts::{Mount, Mounts, MountsError, Unmounted};
pub use path::{PathError, RelPath, normalize_into};
pub use plant::RootsPlant;
pub use roots::{Digest, MOST_ROOTS, NotSorted, Root, RootId, RootKind, Roots, RootsError, listing_digest};

/// The caps of a read list and of a path (D165: a field each, the shipped values one constant,
/// drawn down by the simulation).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most entries one read list holds ([`Full`] past it).
    pub most_entries: u32,
    /// The most bytes of one path ([`PathError::TooLong`] past it); at least 1.
    pub most_path: u32,
    /// The most bytes of path text one read list holds, all entries together.
    pub most_text: u32,
}

impl Limits {
    /// What blaze ships: 65,536 entries (the wasm script host's `read-limit`, 2^16), 4 KiB a
    /// path (`PATH_MAX`), 16 MiB of path text.
    pub const SHIPPED: Limits = Limits { most_entries: 1 << 16, most_path: 4096, most_text: 16 << 20 };
}
