//! `#[macro_export]` macros written by a macro expansion.
//!
//! The Reference, "Macros By Example", "Path-based scope": a `#[macro_export]` macro is placed
//! in the crate root and is named by path from other crates, however it came to be written; a
//! macro-expanded one included (tokio's `select!` is written by its `doc! { .. }` wrapper, and
//! alloy-primitives' `address!` by `fixed_bytes_macros! { .. }`). `#[macro_use] extern crate`
//! brings it in too. Two exports of one name in one crate are an error ("Names", "Name
//! resolution": a name is bound once per namespace in a scope), noted, the first by key kept.

use std::format;

use blaze_surface::{Namespace, PathKind};

use super::entry::{Texts, input, resolved};
use crate::names::Written;
use crate::resolve::NamesNote;
use crate::task::RootNote;

/// The tokio shape: a wrapper writes the export, inside a child module.
const DEP: &str = "#![no_core]
macro_rules! doc { ($m:item) => { #[macro_export] $m }; }
mod inner {
    doc! { macro_rules! select { () => {} } }
    doc! { macro_rules! twice { () => {} } }
}
#[macro_export]
macro_rules! twice { () => {} }
";

/// Names it by path, and through `#[macro_use]`.
const USER: &str = "#![no_core]
use dep::select;
select!();
pub mod used {
    dep::select!();
}
";

const USE_ALL: &str = "#![no_core]
#[macro_use]
extern crate dep;
select!();
";

static TEXTS: Texts = Texts(&[&[("/user/lib.rs", USER)], &[("/dep/lib.rs", DEP)], &[("/all/lib.rs", USE_ALL)]]);

#[test]
fn a_macro_expanded_export_is_a_binding_of_its_crate_root_for_other_crates() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[]), input("all", "/all/lib.rs", &[("dep", 1)])];
    let names = resolved(&crates, &TEXTS);
    let g = alloc_global();
    let select = names
        .item_at(names.root(1), Namespace::Macros, Written { kind: PathKind::Plain, segments: &["select"] }, g)
        .unwrap_or_else(|e| panic!("dep's root binds the written export: {e}"));
    assert_eq!(select.krate, 1);
    for krate in [0, 2] {
        let errors = &names.krate(krate).done.get(&names.root(krate)).expect("the root is done").scope.errors;
        assert!(errors.is_empty(), "crate {krate}: `select` resolves by path and by #[macro_use]: {errors:?}");
    }
    let notes = names.notes(1);
    let twice = notes.iter().filter(|n| matches!(n, NamesNote::Root(RootNote::MacroExportedTwice { name: "twice" }))).count();
    assert_eq!(twice, 1, "`twice` exported by the text and by an expansion is noted once: {}", format!("{notes:?}"));
}

/// commonware-runtime's shape: `stability_scope! { pub mod utils; }` writes the `mod` item,
/// and `utils.rs`'s own text holds a `#[macro_export]` macro. The Reference, "Macros By
/// Example", "Path-based scope": it is placed in the crate root wherever it is written, so
/// `use dep::spawn_cell;` names it from another crate.
const MADE_DEP: &str = "#![no_core]
macro_rules! scope { ($($i:item)*) => { $($i)* }; }
scope! { pub mod utils; }
";

const MADE_UTILS: &str = "#[macro_export]
macro_rules! spawn_cell { () => {} }
";

const MADE_USER: &str = "#![no_core]
use dep::spawn_cell;
spawn_cell!();
";

static MADE_TEXTS: Texts = Texts(&[&[("/user/lib.rs", MADE_USER)], &[("/dep/lib.rs", MADE_DEP), ("/dep/utils.rs", MADE_UTILS)]]);

#[test]
fn an_export_in_a_macro_declared_modules_file_is_a_binding_of_its_crate_root() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &MADE_TEXTS);
    let g = alloc_global();
    names
        .item_at(names.root(1), Namespace::Macros, Written { kind: PathKind::Plain, segments: &["spawn_cell"] }, g)
        .unwrap_or_else(|e| panic!("dep's root binds the export of the made module's file: {e}"));
    let errors = &names.krate(0).done.get(&names.root(0)).expect("the root is done").scope.errors;
    assert!(errors.is_empty(), "`use dep::spawn_cell` resolves: {errors:?}");
}

/// alloy-primitives 1.7.3's shape: `fixed_bytes_macros!` writes `address!`, whose body calls
/// `$crate::hex!`. The Reference, "Macros By Example", "Hygiene": `$crate` names the crate
/// that defines the macro wherever it is called, so the written definition keeps it, and a
/// call from another crate reaches the defining crate's `hex!`, not the caller's. tokio
/// 1.53.2's shape too: `doc! { macro_rules! try_join { .. $crate .. } }`, the `$crate` in
/// the call's own tokens, passed through an `item` fragment.
const DOLLAR_DEP: &str = "#![no_core]
#[macro_export]
macro_rules! hex { () => {} }
macro_rules! fixed_bytes_macros { () => { #[macro_export] macro_rules! address { () => { $crate::hex!(); } } }; }
fixed_bytes_macros!();
macro_rules! doc { ($m:item) => { #[macro_export] $m }; }
doc! { macro_rules! try_join { () => { $crate::hex!(); } } }
";

const DOLLAR_USER: &str = "#![no_core]
dep::address!();
dep::try_join!();
";

static DOLLAR_TEXTS: Texts = Texts(&[&[("/user/lib.rs", DOLLAR_USER)], &[("/dep/lib.rs", DOLLAR_DEP)]]);

#[test]
fn dollar_crate_in_a_written_macro_definition_names_the_defining_crate() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &DOLLAR_TEXTS);
    for krate in [0, 1] {
        let errors = &names.krate(krate).done.get(&names.root(krate)).expect("the root is done").scope.errors;
        assert!(errors.is_empty(), "crate {krate}: `$crate::hex!` names dep's `hex`: {errors:?}");
    }
}

/// fluent-bundle 0.16's shape: a macro of crate `a`, re-exported by `b` and called in `c`,
/// which depends on `b` only. The Reference, "Macros By Example", "Hygiene": `$crate` names
/// the crate that defines the macro, here `a`, wherever the call is, so `$crate::n!` and
/// `$crate::inner::f` go to `a`'s root by `a`'s identity: `c` has no extern name for `a`.
const FAR_A: &str = "#![no_core]
#[macro_export]
macro_rules! m { () => { $crate::n!(); pub use $crate::inner::f; } }
#[macro_export]
macro_rules! n { () => {} }
pub mod inner { pub fn f() {} }
";

const FAR_B: &str = "#![no_core]
pub use a::m;
";

const FAR_C: &str = "#![no_core]
b::m!();
";

static FAR_TEXTS: Texts = Texts(&[&[("/a/lib.rs", FAR_A)], &[("/b/lib.rs", FAR_B)], &[("/c/lib.rs", FAR_C)]]);

#[test]
fn dollar_crate_of_a_crate_that_is_not_a_dependency_of_the_caller_is_the_defining_crate() {
    let crates = [input("a", "/a/lib.rs", &[]), input("b", "/b/lib.rs", &[("a", 0)]), input("c", "/c/lib.rs", &[("b", 1)])];
    let names = resolved(&crates, &FAR_TEXTS);
    let errors = &names.krate(2).done.get(&names.root(2)).expect("the root is done").scope.errors;
    assert!(errors.is_empty(), "`$crate::n!` and `$crate::inner::f` name `a`'s items from `c`: {errors:?}");
    let f = names
        .item_at(names.root(2), Namespace::Values, Written { kind: PathKind::Plain, segments: &["f"] }, alloc_global())
        .unwrap_or_else(|e| panic!("`f`, imported by `pub use $crate::inner::f` in `c`: {e}"));
    assert_eq!(f.krate, 0, "`f` is `a`'s");
}

fn alloc_global() -> blaze_own::Alloc<'static> {
    blaze_own::Alloc::global()
}
