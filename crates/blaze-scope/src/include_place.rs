//! Where an include reads, and whether it left its own package (decision D111, amending D60,
//! 2026-11-16; the owner: "sure allow it but give a warning for include str").
//!
//! The rules. The pinned library, core `macros/mod.rs`, `include!`, `include_str!`,
//! `include_bytes!`: "The file is located relative to the current file (similarly to how
//! modules are found)". Rust bounds the path nowhere; blaze bounds it by the tree a crate is
//! built from, so a key is a function of that tree and nothing of the machine (rules 21, 25):
//! - a WORKSPACE MEMBER (it sees `/workspace/<dir>`, D96) may read any file under the workspace
//!   root (tempo's `crates/precompiles/src/zone_verifier/attestation.rs` reads
//!   `../../../nitro-attestation/testdata/..`); the file is named by its path under that root;
//!   one outside its own package is ALLOWED and REPORTED ([`IncludePlace::outside_package`],
//!   [`LeftPackage`]: a finding at the call, a warning, never an error). A member's proc
//!   macro's read is the same (resolve.rs `Host::run_proc_macro`, from its read list:
//!   [`SeenRoot::left_package`], the path made by [`served_tree`]);
//! - a DEPENDENCY (it sees `/package`) reads only inside its own package, as does a crate whose
//!   environment is not known;
//! - a path leaving that bound, or absolute and not under the root the crate sees for its own
//!   package (`concat!(env!("CARGO_MANIFEST_DIR"), "/x")`, D96 (4)), is `None`: the host
//!   answers `Included::Outside` and the expansion refuses it by its path and the rule
//!   (`IncludeFailure::Outside`, "the path leaves the unit's source root: decision D60").

use blaze_expand::seen::SeenRoot;
use blaze_expand::{ReadRoot, SourceOrigin, include_target};
use blaze_own::{AString, Alloc};
use blaze_surface::CallShape;

/// The file an include reads: its path in the reader's path space (the package's or the
/// workspace's tree, as the texts hold it: `/0/crates/x/data.json`), and whether it lies
/// outside the calling crate's own package.
#[derive(Debug)]
pub struct IncludePlace<'a> {
    pub path: AString<'a>,
    pub outside_package: bool,
}

/// A read of a call that left its own package and stayed inside the workspace (an include, a
/// member's proc macro), told to the sources (`SourceTexts::left_package`) so the names record
/// files it as a finding at the call (blaze-cli names/include_outside.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LeftPackage<'p> {
    /// The calling file's path (absolute in the reader's path space, `/0/crates/x/src/a.rs`).
    pub from: &'p str,
    /// The byte in the calling file where the (topmost) call's path starts.
    pub at: u32,
    /// The called macro's path AS WRITTEN at the call, never resolved: `include_str` for a
    /// builtin, `macros::sol` or `Abi` for a proc macro. So the finding is printed with no
    /// resolution, and two spellings of one macro (`sol`, `alloy::sol`) give two messages.
    pub call: &'p str,
    /// The call's shape: how the message writes `call` (`call!`, `#[call]`, `#[derive(call)]`).
    pub shape: CallShape,
    /// The file read, as [`IncludePlace::path`].
    pub path: &'p str,
}

/// The folder, in the reader's path space, of the tree a crate whose package folder is
/// `package` (`/0/crates/x/`) is served in when it sees `seen` (D96): a dependency's tree is
/// its package (`/package`), a member's the workspace (`/workspace`): its package folder less
/// its dir (`/0`). `None` when the folder does not end in the member's dir (the texts and the
/// environment disagree: nothing is guessed). A path a sandbox records from its tree's root
/// (`crates/y/x.json`) is this folder joined with it: the form [`IncludePlace::path`] has.
pub fn served_tree<'p>(package: &'p str, seen: SeenRoot<'_>) -> Option<&'p str> {
    let package = package.trim_end_matches('/');
    match seen {
        SeenRoot::Package | SeenRoot::Member { dir: "" } => Some(package),
        SeenRoot::Member { dir } => package.strip_suffix(dir)?.strip_suffix('/'),
    }
}

/// The file an include written in the file `calling` (absolute, under the package folder
/// `package`) names by `written`, for a crate that sees `seen`: see the module's doc. `None`
/// when the calling file is not under the package, the path leaves the crate's bound, or it is
/// absolute and not under the root the crate sees for its package. A member whose package
/// folder does not end in its `dir` is `None` too (the texts and the environment disagree:
/// nothing is guessed).
pub fn include_place<'a>(package: &str, seen: Option<SeenRoot<'_>>, calling: &str, written: &str, alloc: Alloc<'a>) -> Option<IncludePlace<'a>> {
    let package = package.trim_end_matches('/');
    let (folder, written) = match seen.and_then(|seen| seen.under(written)) {
        Some(rest) => ("", rest),
        None => {
            let folder = calling.rsplit_once('/').map_or("", |(dir, _)| dir);
            (folder.strip_prefix(package)?, written)
        }
    };
    // The folder the path is spelled from and the bound it may not leave.
    let (base, dir, origin) = match seen {
        Some(seen @ SeenRoot::Member { dir }) => (served_tree(package, seen)?, dir, SourceOrigin::Workspace),
        Some(SeenRoot::Package) | None => (package, "", SourceOrigin::Package),
    };
    let target = include_target(ReadRoot { origin, package: dir }, folder, written, alloc).ok()?;
    let mut own = dir.split('/').filter(|segment| !segment.is_empty());
    let mut ours = target.iter();
    let outside_package = loop {
        match (own.next(), ours.next()) {
            (None, _) => break false,
            (Some(want), Some(have)) if want == *have => {}
            _ => break true,
        }
    };
    let mut path = AString::new_in(alloc);
    path.push_str(base);
    for segment in target.iter() {
        path.push('/');
        path.push_str(segment);
    }
    Some(IncludePlace { path, outside_package })
}
