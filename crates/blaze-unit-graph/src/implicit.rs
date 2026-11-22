//! What a unit is given with nothing in its manifest: the toolchain's crates and its prelude
//! (docs/v2/plan/design/toolchain.md, section 3; std.md, "The blaze edition: opting in").
//! The one home of the rule: the driver, the names library and the ladder ask [`implicit`].
//!
//! | unit | `core` | `alloc` | `std` names | prelude |
//! |---|---|---|---|---|
//! | Rust edition | yes | by `extern crate alloc` | Rust's `std` | `std::prelude::rust_20xx` |
//! | blaze edition (lib, bin, test) | yes | by `extern crate alloc` | bstd; Rust's `std` none | bstd's `prelude::blaze_2026_11` |
//! | its build script, a proc-macro target | as a Rust edition unit | | | |
//! | a proc macro | also `proc_macro` | | | |
//!
//! A blaze-edition binary (a `bin` or an example) is also given its entry ([`Implicit::entry`],
//! D104): its root's `fn main` is the input of `std::main` (bstd's `#[bstd::main]`) with no
//! attribute written, so the parameters it names are its grants.
//!
//! bstd has one extern name, `std`; `bstd::..` in a blaze-edition crate is a finding (names'
//! Cli 8), never a second name (rule 30). There is no opt-in to Rust's `std` under the edition.
//! The toolchain's own crates (core, alloc, std and what they are built from) are not given
//! this: they say `#![no_core]`/`#![no_std]` and name what they use in their manifests.

use edition::{BlazeEdition, Edition, PackageEdition};

use crate::UnitKind;

/// A unit of the toolchain's graph for a target: its index in the toolchain manifest's unit
/// list (blaze-toolchain `ToolchainView::units`), never a [`crate::UnitId`] of the workspace.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ToolchainUnit(pub u32);

/// The toolchain's library units for one target and side (target, or host for build scripts
/// and proc macros), as the driver found them in the manifest by role.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sysroot {
    pub core: ToolchainUnit,
    pub alloc: ToolchainUnit,
    /// Rust's `std`.
    pub std: ToolchainUnit,
    pub proc_macro: ToolchainUnit,
    pub bstd: ToolchainUnit,
}

/// Which `std` a unit's code names `std`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StdIs {
    /// Rust's `std`.
    Rust,
    /// bstd, under the blaze edition.
    Bstd(BlazeEdition),
}

/// The prelude a unit's modules have in scope: a module of its `std`'s `prelude`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prelude {
    /// `std::prelude::rust_2015` .. `rust_2024`.
    Rust(Edition),
    /// bstd's `prelude::blaze_2026_11`.
    Blaze(BlazeEdition),
}

impl Prelude {
    /// The module's name within `std::prelude`.
    pub const fn module(self) -> &'static str {
        match self {
            Prelude::Rust(Edition::Edition2015) => "rust_2015",
            Prelude::Rust(Edition::Edition2018) => "rust_2018",
            Prelude::Rust(Edition::Edition2021) => "rust_2021",
            Prelude::Rust(Edition::Edition2024) => "rust_2024",
            Prelude::Blaze(blaze) => blaze.prelude(),
        }
    }
}

/// What a unit is given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Implicit {
    pub std: StdIs,
    pub prelude: Prelude,
    /// `proc_macro` in its extern prelude: a proc-macro unit.
    pub proc_macro: bool,
    /// D104: the root's `fn main` is expanded by `std::main` unasked (a blaze-edition binary:
    /// [`Implicit::binary`]). The names library reads it (blaze-scope `LowerCx::entry`).
    pub entry: bool,
}

/// What a unit of `edition` and `kind` is given. A build script and a proc-macro target are
/// Rust edition units of the edition's syntax, whatever the package's edition (std.md: the
/// edition's attribute is not put on them).
pub fn implicit(edition: PackageEdition, kind: &UnitKind<'_>) -> Implicit {
    let host_side = matches!(kind, UnitKind::BuildScript | UnitKind::ProcMacro);
    let (std, prelude) = match edition.blaze {
        Some(blaze) if !host_side => (StdIs::Bstd(blaze), Prelude::Blaze(blaze)),
        _ => (StdIs::Rust, Prelude::Rust(edition.syntax)),
    };
    let it = Implicit { std, prelude, proc_macro: matches!(kind, UnitKind::ProcMacro), entry: false };
    match kind {
        UnitKind::Bin(_) | UnitKind::Example(_) => it.binary(),
        _ => it,
    }
}

impl Implicit {
    /// What a binary of this unit's edition is given: under a blaze edition, its entry
    /// ([`Implicit::entry`]); a Rust edition's binary writes its own `fn main`. The one home of
    /// the rule: [`implicit`] asks it for a `bin` or an example, and a caller that builds a
    /// program crate without a unit kind (blaze-ladder's program crate) asks it directly.
    pub const fn binary(self) -> Implicit {
        Implicit { entry: matches!(self.std, StdIs::Bstd(_)), ..self }
    }

    /// The extern prelude it adds, sorted by name: (name, toolchain unit). `alloc` is not in
    /// it; [`Implicit::extern_crate`] gives it to `extern crate alloc;`.
    pub fn externs(&self, sysroot: &Sysroot) -> impl Iterator<Item = (&'static str, ToolchainUnit)> {
        let std = match self.std {
            StdIs::Rust => sysroot.std,
            StdIs::Bstd(_) => sysroot.bstd,
        };
        let proc_macro = self.proc_macro.then_some(("proc_macro", sysroot.proc_macro));
        [Some(("core", sysroot.core)), proc_macro, Some(("std", std))].into_iter().flatten()
    }

    /// The toolchain crate `extern crate <name>;` names beyond the extern prelude: `alloc`,
    /// and `proc_macro` (which any crate may name so, as rustc allows).
    pub fn extern_crate(&self, name: &str, sysroot: &Sysroot) -> Option<ToolchainUnit> {
        match name {
            "alloc" => Some(sysroot.alloc),
            "proc_macro" => Some(sysroot.proc_macro),
            _ => self.externs(sysroot).find(|(it, _)| *it == name).map(|(_, unit)| unit),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::*;

    const SYSROOT: Sysroot = Sysroot {
        core: ToolchainUnit(0),
        alloc: ToolchainUnit(1),
        std: ToolchainUnit(2),
        proc_macro: ToolchainUnit(3),
        bstd: ToolchainUnit(4),
    };

    fn externs(edition: &str, kind: &UnitKind<'_>) -> Vec<(&'static str, u32)> {
        let edition = PackageEdition::of_manifest(edition).expect("an edition");
        implicit(edition, kind).externs(&SYSROOT).map(|(name, unit)| (name, unit.0)).collect()
    }

    #[test]
    fn the_blaze_edition_names_bstd_std_and_rust_std_nothing() {
        let blaze = PackageEdition::of_manifest("blaze-2026.11").expect("the edition");
        for kind in [UnitKind::Lib, UnitKind::Test { bin: None }] {
            let it = implicit(blaze, &kind);
            assert_eq!(it.std, StdIs::Bstd(BlazeEdition::Blaze2026_11));
            assert_eq!(it.prelude.module(), "blaze_2026_11");
            assert_eq!(externs("blaze-2026.11", &kind), [("core", 0), ("std", 4)]);
            // No name reaches Rust's std (unit 2).
            for name in ["std", "core", "alloc", "proc_macro", "bstd"] {
                assert_ne!(it.extern_crate(name, &SYSROOT), Some(SYSROOT.std), "{name}");
            }
            assert_eq!(it.extern_crate("bstd", &SYSROOT), None);
            assert_eq!(it.extern_crate("alloc", &SYSROOT), Some(SYSROOT.alloc));
        }
    }

    /// D104: only a blaze-edition binary's root `fn main` is `std::main`'s input. A library, a
    /// test harness, a build script and a proc macro of the edition are not entries, nor is any
    /// unit of a Rust edition (its `fn main` is Rust's). A planted `entry: true` on `Lib` fails
    /// the first loop.
    #[test]
    fn a_blaze_binary_is_an_entry_and_nothing_else_is() {
        let blaze = PackageEdition::of_manifest("blaze-2026.11").expect("the edition");
        let rust = PackageEdition::of_manifest("2024").expect("an edition");
        for kind in [UnitKind::Lib, UnitKind::Test { bin: None }, UnitKind::BuildScript, UnitKind::ProcMacro] {
            assert!(!implicit(blaze, &kind).entry, "{kind:?} of the blaze edition is no entry");
        }
        assert!(implicit(blaze, &UnitKind::Lib).binary().entry, "a blaze program crate is one");
        assert!(!implicit(rust, &UnitKind::Lib).binary().entry, "a Rust binary writes its own main");
        assert!(!implicit(blaze, &UnitKind::BuildScript).binary().entry, "a build script is a Rust unit");
    }

    #[test]
    fn build_scripts_and_proc_macros_are_rust_units() {
        assert_eq!(externs("blaze-2026.11", &UnitKind::BuildScript), [("core", 0), ("std", 2)]);
        assert_eq!(externs("blaze-2026.11", &UnitKind::ProcMacro), [("core", 0), ("proc_macro", 3), ("std", 2)]);
        let blaze = PackageEdition::of_manifest("blaze-2026.11").expect("the edition");
        assert_eq!(implicit(blaze, &UnitKind::BuildScript).prelude, Prelude::Rust(Edition::Edition2024));
    }

    #[test]
    fn rust_editions_get_rust_std_and_their_prelude() {
        for (written, module) in [("2015", "rust_2015"), ("2018", "rust_2018"), ("2021", "rust_2021"), ("2024", "rust_2024")] {
            assert_eq!(externs(written, &UnitKind::Lib), [("core", 0), ("std", 2)]);
            let edition = PackageEdition::of_manifest(written).expect("an edition");
            assert_eq!(implicit(edition, &UnitKind::Lib).prelude.module(), module);
        }
    }
}
