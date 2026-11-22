//! What of a checkout a build script is never served, and the planner never walks: the folders
//! at the checkout's root that are no package's. THE ONE HOME of that rule (rule 30).
//!
//! - blaze's own folder, `.blaze`, by its name: blaze makes it (the profile every command
//!   writes, `.blaze/v2/profile.txt`; blaze-cli clean.rs `CheckoutEntry`), so a run that read it
//!   would hold a read blaze itself moves ("changed while the command ran", turso c1851p).
//! - cargo's build folder, `target`, ONLY when it is one: cargo writes `CACHEDIR.TAG` in it
//!   (cargo `paths::create_dir_all_excluded_from_backups_atomically`), whose first line is the
//!   Cache Directory Tagging spec's signature, `Signature: 8a477f597d28d172789f06886806bc55`.
//!   A root folder named `target` without that tag is a package's folder like any other and is
//!   served (rule 4: no valid program is mis-served by a name guess).
//!
//! A folder not served is absent to a script: not listed, opened or looked at. The tag is read
//! wherever a path under `target` is answered, so each read's recorded answer is a function of
//! it: tagging the folder later moves the answer of every read that saw it (the root's listing,
//! a file in it), and the read list sees the change.

#![blaze::sans_io]

/// blaze's folder in the checkout's root.
pub const BLAZE_FOLDER: &str = ".blaze";

/// The name cargo's build folder has in the checkout's root (`Workspace::default_target_dir`).
pub const CARGO_TARGET: &str = "target";

/// The tag file cargo writes in its build folder.
pub const CACHEDIR_TAG: &str = "CACHEDIR.TAG";

/// The first line of a Cache Directory Tagging file (bford.info/cachedir: "the file must begin
/// with the following 43-byte header string").
pub const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// What a name at the checkout's root is to this rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RootName {
    /// blaze's folder: never served.
    Blaze,
    /// Named `target`: not served when its [`CACHEDIR_TAG`] is cargo's ([`tagged`]).
    Target,
    /// Any other name: served.
    Other,
}

/// What `name`, an entry of the checkout's root, is.
pub fn root_name(name: &[u8]) -> RootName {
    if name == BLAZE_FOLDER.as_bytes() {
        RootName::Blaze
    } else if name == CARGO_TARGET.as_bytes() {
        RootName::Target
    } else {
        RootName::Other
    }
}

/// Whether the bytes of `target/CACHEDIR.TAG` (`None`: no such file) mark a cache folder: they
/// begin with [`CACHEDIR_SIGNATURE`]. The spec's header is the line's whole first 43 bytes; what
/// follows it is the tag's own text.
pub fn tagged(tag: Option<&[u8]>) -> bool {
    tag.is_some_and(|bytes| bytes.starts_with(CACHEDIR_SIGNATURE))
}

/// Whether the root entry `name` is served, `tag` being the bytes of `target/CACHEDIR.TAG` (the
/// first [`CACHEDIR_SIGNATURE`]`.len()` are enough; read only when [`root_name`] is
/// [`RootName::Target`], `None` otherwise).
pub fn served_at_root(name: &[u8], tag: Option<&[u8]>) -> bool {
    match root_name(name) {
        RootName::Blaze => false,
        RootName::Target => !tagged(tag),
        RootName::Other => true,
    }
}

// ---------------------------------------------------------------------------------------------
// A package's files: what a walk of ONE package's folder takes (THE ONE HOME of that rule too,
// rule 30). Taken by every package walk: blaze-proc-macro `module::package_files` (a path macro
// crate's `files_digest`), blaze-cli macro_files.rs (a member macro crate's `MacroFiles` row,
// the same digest) and the bootstrap builder's `files_digest` (scripts proc_macro_modules.rs).
//
// The rule, from what cargo packages and fingerprints a path package by (`PathSource::
// list_files`; the Cargo Book, "The `exclude` and `include` fields": "A directory named `target`
// in the root of the package will be skipped"), kept a SUPERSET of it wherever cargo's answer
// depends on what blaze does not read (git's index), because the set is a module key's input
// and a key must miss no input (rule 21):
// - a version-control system's own entry ([`VCS_ENTRIES`]: `.git`, a folder or a worktree's
//   link file, and the others) is never a package's, at any depth: cargo never lists it;
// - at the package's root, a folder named `target` is cargo's build folder: skipped by its
//   name, as cargo skips it (no tag is read: cargo packages no root `target` either way);
// - at the package's root, blaze's own folder `.blaze` (it is only ever the checkout's, which
//   a root package's folder is);
// - everything else is walked: dotfiles and dot-folders (`.cargo/`, `.github/`; cargo packages
//   the ones git tracks, and blaze cannot ask git's index, so all of them), and a `target`
//   folder beneath the root (a source folder like any other).
// Before 2026-11-21 the three walkers each wrote "no folder named `target` or starting with `.`,
// at any depth", which left out an input (an `include_str!` of a file in `.data/`, a module in
// `src/target/`).

/// The names a version-control system keeps its own state under, in a package's folder or
/// beneath it: git (a folder, or a worktree's or submodule's link file), Jujutsu, Mercurial,
/// Pijul, Subversion, Bazaar and Darcs.
pub const VCS_ENTRIES: [&str; 7] = [".git", ".jj", ".hg", ".pijul", ".svn", ".bzr", "_darcs"];

/// Where an entry of a package walk is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InPackage {
    /// In the package's own folder.
    Root,
    /// In a folder beneath it.
    Beneath,
}

/// Whether a package walk takes the entry `name` (`is_dir`: a folder, else a file) found `at`:
/// a folder taken is walked into, a regular file taken is one of the package's files. The rule
/// is this module's second half (above).
pub fn in_package(name: &[u8], is_dir: bool, at: InPackage) -> bool {
    if VCS_ENTRIES.iter().any(|vcs| name == vcs.as_bytes()) {
        return false;
    }
    match (at, is_dir) {
        (InPackage::Root, true) => name != CARGO_TARGET.as_bytes() && name != BLAZE_FOLDER.as_bytes(),
        (InPackage::Root, false) | (InPackage::Beneath, _) => true,
    }
}

#[cfg(test)]
mod package_tests {
    use super::{InPackage, in_package};

    /// The package rule, each case with its twin: a root `target` folder is left out and a
    /// nested one walked; a VCS folder is left out at any depth and a dot-folder walked.
    /// Prints nothing; passes as `.. served::package_tests::the_package_rule ... ok`.
    #[test]
    fn the_package_rule() {
        let cases: [(&[u8], bool, InPackage, bool); 10] = [
            (b"target", true, InPackage::Root, false),
            (b"target", true, InPackage::Beneath, true),
            (b"target", false, InPackage::Root, true),
            (b".blaze", true, InPackage::Root, false),
            (b".git", true, InPackage::Beneath, false),
            (b".git", false, InPackage::Root, false),
            (b".jj", true, InPackage::Root, false),
            (b".cargo", true, InPackage::Root, true),
            (b".gitignore", false, InPackage::Root, true),
            (b"src", true, InPackage::Root, true),
        ];
        for (name, is_dir, at, taken) in cases {
            assert_eq!(in_package(name, is_dir, at), taken, "{} (folder: {is_dir}, {at:?})", core::str::from_utf8(name).unwrap_or("?"));
        }
    }
}
