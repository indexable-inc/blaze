//! Where an `include!` reads, and where the modules its file writes are found.
//!
//! The rules. The pinned library, core `macros/mod.rs`, `include!`: "Parses a file as an
//! expression or an item according to the context. The file is located relative to the current
//! file (similarly to how modules are found)." The Reference, "Modules", "Module Source
//! Filenames": "A module without a body is loaded from an external file. When the module does
//! not have a `path` attribute, the path to the file mirrors the logical module path. Ancestor
//! module path components are directories, and the module's contents are in a file with the
//! name of the module plus the `.rs` extension."
//!
//! The Reference has no sentence for a `mod x;` an INCLUDED file writes. What real crates rest
//! on (libp2p-identity 0.2: `mod proto { include!("generated/mod.rs"); .. }` in `src/lib.rs`,
//! whose `generated/mod.rs` writes `pub mod keys_proto;` beside itself, at
//! `src/generated/keys_proto.rs`) is that the included file is the current file of what it
//! writes: its folder is where its `mod x;` is looked for (`x.rs`, then `x/mod.rs`) and what
//! its `#[path]` is relative to, whatever module the `include!` call stands in. An inline
//! `mod y { mod z; }` in the included file then looks under that folder's `y/`, as in any file.

use blaze_expand::seen::SeenRoot;
use blaze_expand::{ReadAnswer, ReadKind, Reads};
use blaze_own::{AString, Alloc};

/// The absolute path of the file an `include!` written in the file `calling` (absolute, under
/// the package folder `package`, the reader's path space) names by `written`: the calling
/// file's folder joined with the written path, `.` and `..` taken lexically
/// ([`include_target`]). An absolute `written` under the root the crate SEES (`seen`, D96 (4):
/// `/package/..` for a dependency, `/workspace/<dir>/..` for a member, what
/// `env!("CARGO_MANIFEST_DIR")` gives) is the package's file at the rest of the path; any other
/// absolute path names the machine and is outside. `None` when the calling file is not under
/// the package or the path leaves it (the texts hold the package alone).
///
/// Decision D111 (amending D60): a workspace member's include may read under the workspace
/// root, a dependency's only under its package: [`crate::include_place::include_place`], the
/// one home of the rule; this is its path alone.
pub fn included_file<'a>(package: &str, seen: Option<SeenRoot<'_>>, calling: &str, written: &str, alloc: Alloc<'a>) -> Option<AString<'a>> {
    crate::include_place::include_place(package, seen, calling, written, alloc).map(|place| place.path)
}

/// The file one `include!` call read, from the call's own read list: the one file read as text
/// that was there. It is the current file of every call its text writes (an `include!` in it
/// reads relative to it: icu_properties_data 2.3.0's `src/lib.rs` includes `data/mod.rs`, which
/// includes `property_*.rs.data` beside itself), and its folder is where a `mod x;` at its top
/// is found. `None` when the call read no file that was there (its diagnostic says so), read
/// more than one (not an `include!` of one path: nothing is guessed), or read a file that is
/// not under the package (a build script's output: the calling file and module folder stay).
pub(crate) fn included_read<'a>(package: &str, seen: Option<SeenRoot<'_>>, calling: &str, reads: &Reads<'_>, alloc: Alloc<'a>) -> Option<AString<'a>> {
    let mut found = reads.list().iter().filter(|read| read.kind == ReadKind::IncludeStr && matches!(read.answer, ReadAnswer::Digest(_)));
    let read = found.next()?;
    if found.next().is_some() {
        return None;
    }
    included_file(package, seen, calling, reads.subject(read), alloc)
}
