//! Which toolchain crates a program is given: decided from its crate root's own attributes
//! BEFORE any toolchain pack is opened, so a `#![no_core]` program opens none.
//!
//! The rule (the Rust Reference, "Preludes", the extern prelude: "If the `no_std` attribute is
//! specified, then the `std` crate is not added to the extern prelude" and core is; rustc's
//! unstable `no_core` attribute, which the Reference leaves out as unstable, takes `core` away
//! too: the crate's root links no crate it does not name). blaze-scope's graph.rs follows it for
//! names (`attrs.no_core` drops every implicit extern); this module makes the LIBRARY SET follow
//! it as well: before it, `run::run_program` opened and verified every pack of bstd's closure
//! for a program that can name none of them (the `3-hello` rung past 60 s).
//!
//! A `no_core` crate may still write `extern crate core;` (graph.rs offers `core`, `alloc` and
//! `std` to `extern crate`): when its root file does, the toolchain is opened as before. An
//! `extern crate` written only in a module file other than the root is not seen here: the
//! names library then refuses that `extern crate` by name (no crate of that name), never a
//! wrong program.

use blaze_own::Alloc;
use blaze_surface::ItemKind;

use crate::crates::{DirShelf, Toolchain, syntax_of};
use crate::refusal::SourceFile;
use crate::run::{NewFront, RunError};

/// The toolchain crates a program may name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Library {
    /// None: a `#![no_core]` root that names no toolchain crate by `extern crate`, or no shelf.
    None,
    /// The toolchain's library on the shelf (`crates::Toolchain::open`).
    Shelf,
}

/// The crates the toolchain extern rule offers to `extern crate` (blaze-scope graph.rs).
const NAMEABLE: [&str; 3] = ["core", "alloc", "std"];

/// What the program of `files` rooted at `root` is given, by its root's attributes under the
/// target's cfg (a `cfg_attr(.., no_core)` counts as written when its predicate holds).
/// `alloc` holds the root's outline for the length of the call.
/// `panic` is the program's strategy (its inputs' carrier): the root's `cfg(panic)` is derived
/// from it, never the target list's.
pub fn library_of(files: &[SourceFile<'_>], root: &str, new: NewFront<'_>, panic: blaze_build::Panic, alloc: Alloc<'_>) -> Result<Library, RunError> {
    if new.shelf.is_none() {
        return Ok(Library::None);
    }
    let file = files.iter().find(|f| f.path == root).ok_or(RunError::NoMain)?;
    let edition = edition::PackageEdition::of_manifest(new.edition).map_err(|_| RunError::Edition { written: new.edition.into() })?;
    let target = blaze_scope::task::Target::AARCH64_APPLE_DARWIN;
    let target_cfg = blaze_scope::task::Cfg::of(&target, &[], alloc);
    let mut atoms: Vec<(&str, Option<&str>)> = target_cfg.atoms().to_vec();
    crate::panic_strategy::derive_cfg(&mut atoms, panic);
    let cfg = blaze_scope::task::Cfg::from_atoms(&atoms, alloc);
    Ok(if root_names_toolchain(file.text, syntax_of(edition), &cfg, alloc) { Library::Shelf } else { Library::None })
}

/// Whether a crate whose root file is `text` (of syntax `edition`, read under `cfg`) may name
/// a toolchain crate: any root that is not `#![no_core]`, and a `no_core` root that writes
/// `extern crate core|alloc|std`. The one rule [`library_of`] and blaze-cli's names task (which
/// sysroot crates a workspace crate is handed) both take, so the run and the check agree.
pub fn root_names_toolchain(text: &str, edition: blaze_surface::Edition, cfg: &blaze_scope::task::Cfg<'_, '_>, alloc: Alloc<'_>) -> bool {
    let outline = blaze_file_items::outline_auto_extensions(text, edition, blaze_file_items::Bodies::Skipped, alloc);
    let attrs = blaze_scope::task::file_attrs(cfg, &outline, text, alloc);
    if !attrs.no_core {
        return true;
    }
    outline.items().iter().any(|item| item.kind() == ItemKind::ExternCrate && item.name.is_some_and(|name| NAMEABLE.contains(&item.text(text, name))))
}

/// The toolchain [`library_of`] decides: opened from `new.shelf` only for [`Library::Shelf`].
pub fn toolchain_for(files: &[SourceFile<'_>], root: &str, new: NewFront<'_>, panic: blaze_build::Panic, alloc: Alloc<'_>) -> Result<Toolchain, RunError> {
    match (library_of(files, root, new, panic, alloc)?, new.shelf) {
        (Library::Shelf, Some(shelf)) => Toolchain::open(&mut DirShelf::new(shelf.into())).map_err(RunError::Crates),
        _ => Ok(Toolchain::none(alloc)),
    }
}

#[cfg(test)]
mod tests {
    use blaze_build::OverflowChecks;
    use blaze_own::Alloc;

    use super::{Library, library_of};
    use crate::refusal::SourceFile;
    use crate::run::NewFront;

    fn new(shelf: Option<&str>) -> NewFront<'_> {
        NewFront { package: "p", edition: "2024", shelf, overflow: OverflowChecks::On }
    }

    fn of(text: &str, shelf: Option<&str>) -> Library {
        let files = [SourceFile { path: "src/main.rs", text }];
        // The roots here write no `cfg(panic)`: the strategy is named, Rust's default.
        library_of(&files, "src/main.rs", new(shelf), blaze_build::Panic::Unwind, Alloc::global()).expect("decided")
    }

    /// A `#![no_core]` root that names no toolchain crate is given none, even with a shelf
    /// (the `3-hello` rung): expected `Library::None`. Without this module, `run_program`
    /// opened the shelf whenever one was given, which this would answer `Library::Shelf`.
    /// A plain root, a `no_core` root that writes `extern crate core;`, and a root whose
    /// `cfg_attr` turns `no_core` on are the controls.
    #[test]
    fn a_no_core_root_is_given_no_toolchain_crate_it_did_not_name() {
        let shelf = Some("/shelf");
        assert_eq!(of("#![feature(no_core, lang_items)]\n#![no_core]\n#[lang = \"sized\"] trait Sized {}\nfn main() {}\n", shelf), Library::None);
        assert_eq!(of("fn main() {}\n", shelf), Library::Shelf);
        assert_eq!(of("#![no_core]\nextern crate core;\nfn main() {}\n", shelf), Library::Shelf);
        assert_eq!(of("#![cfg_attr(target_os = \"macos\", no_core)]\nfn main() {}\n", shelf), Library::None);
        assert_eq!(of("fn main() {}\n", None), Library::None);
    }
}
