//! The macro side's preludes are its own side's (names-G4-k1-2; tempo l3d 19:57: 20 false
//! `Ambiguous(GlobVsOuter)`, libc 0.2.190's macro-side copy `assert!` at src/types.rs:144,
//! `Default!` and `panic!` in src/wasi/mod.rs, serde_core's `Debug` derive).
//!
//! The real shape: libc's `prelude!()` writes `mod prelude { pub(crate) use core::{assert,
//! panic, ..}; pub(crate) use fmt::Debug; pub(crate) use core::default::Default; .. }` and its
//! modules write `use crate::prelude::*;`, under `#![no_std]`. On the macro side (D89 (a))
//! `core` in the extern prelude is the side's core (an explicit dependency), while the injected
//! `#[macro_use] extern crate core;` and the standard prelude were the graph's sysroot's, the
//! target side's core: every glob-imported macro then had a different outer candidate.
//!
//! The rule (the Reference, `names.resolution.expansion.imports.ambiguity.glob-vs-outer`):
//! "Names in imports and macro invocations may not be resolved through glob imports when
//! there is another candidate available in an outer scope"; a candidate that is the same
//! definition is no other candidate (`...glob-vs-glob`: "the imports are of the same item
//! (following reexports)"; D57). The preludes ARE outer scopes for this rule; the error came
//! from the preludes naming another crate than the crate's own `core`. Rust's injection names
//! its crate as any `extern crate` does, through the crate's externs (the Reference, "Extern
//! prelude": "The `core` crate is always added to the extern prelude. The `std` crate is added
//! as long as the `no_std` attribute is not specified in the crate root"; "Preludes", `no_std`:
//! the standard prelude is then `core`'s): so the side's `core` is the side's preludes'.

use blaze_manifest::lock::{Lock, LockId};
use blaze_surface::Namespace;
use blaze_toolchain::ToolchainView;

use super::macro_side_names::{HOST, LOCK, a, graph, index_of, toolchain};
use super::{Member, UnitCrates, unit_crates};
use crate::resolve::{Names, SourceTexts, resolve_crates};
use crate::task::Serial;

/// Every `core` (the target side's and the macro side's): `core`'s own shapes of the four
/// macros and its standard prelude (library/core: `#[macro_export] macro_rules! panic`, the
/// built-in `assert`, `fmt::macros::Debug` re-exported as `fmt::Debug`, `default::Default`,
/// `prelude::rust_2021` re-exporting them).
const CORE: &str = "#![no_core]
#[macro_export]
macro_rules! assert { () => {} }
#[macro_export]
macro_rules! panic { () => {} }
pub mod fmt {
    pub use self::macros::Debug;
    pub mod macros {
        pub macro Debug($item:item) {}
    }
}
pub mod default {
    pub macro Default($item:item) {}
}
pub mod prelude {
    pub mod rust_2021 {
        pub use crate::{assert, panic};
        pub use crate::fmt::macros::Debug;
        pub use crate::default::Default;
    }
    pub mod rust_2024 {
        pub use super::rust_2021::*;
    }
}
";

/// libc's shape on both sides: a `#![no_std]` crate whose own `prelude` module re-exports
/// core's macros, globbed into the root, each name then used (one-segment imports: the same
/// lookup as a macro call's first segment, `first_segment`).
const DEP: &str = "#![no_std]
mod prelude {
    pub(crate) use core::{assert, panic};
    pub(crate) use core::fmt::Debug;
    pub(crate) use core::default::Default;
}
use crate::prelude::*;
use assert as checked;
use panic as panicked;
use Debug as Shown;
use Default as Made;
";

const EMPTY: &str = "#![no_core]
";

/// Each crate's one file by its name: `core` and `dep` as above, every other crate empty.
struct Texts<'c> {
    roots: blaze_own::AVec<'c, (&'c str, &'static str)>,
}

impl<'c> Texts<'c> {
    fn of(crates: &'c UnitCrates<'_>, dep: &'static str) -> Texts<'c> {
        let mut roots = blaze_own::AVec::with_capacity_in(crates.crates.len(), a());
        for c in crates.crates.iter() {
            let text = match c.name {
                "core" => CORE,
                "dep" => dep,
                _ => EMPTY,
            };
            roots.push((c.root.as_str(), text));
        }
        Texts { roots }
    }
}

impl<'c> SourceTexts<'c> for Texts<'c> {
    fn read(&self, krate: usize, path: &str) -> Option<&'c str> {
        let (root, text) = self.roots.get(krate)?;
        (*root == path).then_some(*text)
    }
}

/// The crate the macro-namespace binding `name` of crate `krate`'s root names, if any.
fn bound_in(names: &Names<'_>, krate: usize, name: &'static str) -> Option<usize> {
    names.lookup(names.root(krate), Namespace::Macros, name).and_then(|b| names.crate_of(b.def.key))
}

/// Expected line: `test units::macro_side_prelude::a_macro_side_glob_of_cores_macros_beside_the_preludes_is_no_ambiguity ... ok`.
///
/// On the macro side, `dep`'s `checked`, `panicked`, `Shown` and `Made` are bound, each to the
/// macro side's `core` (one candidate: the glob's and the preludes' are the same definition).
/// Before names-G4-k1-2 each import failed `Ambiguous(GlobVsOuter)` and bound nothing
/// ("`checked` on the macro side: None"). The control: the target side's `dep`, the same text
/// with its preludes and its `core` one crate, binds each to the target's `core`, before and
/// after.
#[test]
fn a_macro_side_glob_of_cores_macros_beside_the_preludes_is_no_ambiguity() {
    let lock = Lock::read(LOCK.as_bytes(), a()).expect("the lock");
    let members = [Member::new(LockId(0), "m0").expect("a plain dir"), Member::new(LockId(2), "m2").expect("a plain dir")];
    let g = graph();
    let bytes = toolchain(true);
    let view = ToolchainView::open(&bytes, ToolchainView::id_of_bytes(&bytes)).expect("its own id");
    let crates = unit_crates(&g, &lock, &members, view, HOST, a()).expect("the crates");
    assert!(crates.refused.is_empty(), "with the cut, nothing is refused");

    let target_dep = index_of(&crates, "dep", 0);
    let pm = index_of(&crates, "pm", 0);
    let side_dep = index_of(&crates, "dep", pm + 1);
    assert!(side_dep >= crates.macro_from && target_dep < crates.macro_from, "one `dep` per side");
    let core_of = |at: usize| crates.crates[at].name == "core";

    let texts = Texts::of(&crates, DEP);
    let inputs = crates.inputs();
    let names = resolve_crates(&inputs, crates.sysroot, &texts, &Serial, crate::tests::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    for name in ["checked", "panicked", "Shown", "Made"] {
        let side = bound_in(&names, side_dep, name);
        assert!(
            side.is_some_and(|c| core_of(c) && c >= crates.macro_from),
            "`{name}` on the macro side: {side:?}, the rule: the macro side's `core` (notes {:?})",
            names.notes(side_dep)
        );
        let target = bound_in(&names, target_dep, name);
        assert!(
            target.is_some_and(|c| core_of(c) && c < crates.macro_from),
            "the control, `{name}` on the target side: {target:?}, the rule: the target's `core` (notes {:?})",
            names.notes(target_dep)
        );
    }
}

/// alloy 2.5.0's root (src/lib.rs:16-20) in a 2021 `#![no_std]` crate: `pub use alloy_core as
/// core; pub use self::core::primitives;` (here `alloy_core` is a module of its own). tempo's
/// precompiles-macros takes alloy with `default-features = false`, so its host unit has no
/// `std` and no `#![no_std]` lifted: "no `primitives` in `alloy`" x3.
const SHADOWS_CORE: &str = "#![no_std]
pub mod alloy_core {
    pub mod primitives {
        pub struct U256;
    }
}
pub use self::alloy_core as core;
pub use self::core::primitives;
";

/// The control: the same root with `std` injected (no `#![no_std]`), so no injected `core`.
const SHADOWS_CORE_STD: &str = "pub mod alloy_core {
    pub mod primitives {
        pub struct U256;
    }
}
pub use self::alloy_core as core;
pub use self::core::primitives;
";

/// The crate of the type-namespace binding `name` in crate `krate`'s root, if any.
fn type_in(names: &Names<'_>, krate: usize, name: &'static str) -> Option<usize> {
    names.lookup(names.root(krate), Namespace::Types, name).and_then(|b| names.crate_of(b.def.key))
}

/// Expected line: `test units::macro_side_prelude::a_written_core_stands_beside_the_injected_one_from_2018 ... ok`.
///
/// From 2018 the injected `extern crate core;` is def-site (rustc's `std_inject`), so a root's
/// own `core` stands and `self::core::primitives` is the root's. Before: the injected item won
/// over the import (an item beside an import stands), `self::core` was the toolchain's `core`
/// and `primitives` was unbound (`None`). CONTROL: with `std` injected there is no second
/// `core`, and `primitives` is bound before and after.
#[test]
fn a_written_core_stands_beside_the_injected_one_from_2018() {
    let lock = Lock::read(LOCK.as_bytes(), a()).expect("the lock");
    let members = [Member::new(LockId(0), "m0").expect("a plain dir"), Member::new(LockId(2), "m2").expect("a plain dir")];
    let g = graph();
    let bytes = toolchain(true);
    let view = ToolchainView::open(&bytes, ToolchainView::id_of_bytes(&bytes)).expect("its own id");
    let crates = unit_crates(&g, &lock, &members, view, HOST, a()).expect("the crates");
    let pm = index_of(&crates, "pm", 0);
    let deps = [index_of(&crates, "dep", 0), index_of(&crates, "dep", pm + 1)];
    for (text, what) in [(SHADOWS_CORE, "no_std"), (SHADOWS_CORE_STD, "the control, std")] {
        let texts = Texts::of(&crates, text);
        let inputs = crates.inputs();
        let names = resolve_crates(&inputs, crates.sysroot, &texts, &Serial, crate::tests::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        for dep in deps {
            let bound = type_in(&names, dep, "primitives");
            assert_eq!(bound, Some(dep), "{what}: `primitives` in dep #{dep}'s root is its own module (notes {:?})", names.notes(dep));
        }
    }
}
