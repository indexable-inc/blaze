//! An `include!` written in an INCLUDED file reads relative to that file, and the
//! `#[macro_export]` macros and `pub use` re-exports it brings in are the crate's.
//!
//! Reduced from icu_properties_data 2.3.0, the 135 unresolved macro paths of icu_properties in
//! tempo's names census (2026-11-16, "macro NotFound: `impl_property_binary_alnum_v1`"):
//! `src/lib.rs` writes `include!("../data/mod.rs");`, `data/mod.rs` is about 140
//! `include!("property_*.rs.data");`, and each `.rs.data` file holds `#[macro_export]
//! macro_rules! __impl_property_binary_alnum_v1 { .. }` and `pub use
//! __impl_property_binary_alnum_v1 as impl_property_binary_alnum_v1;`. icu_properties writes
//! `use icu_properties_data::*;` and then `impl_property_binary_alnum_v1!(Baked);`.
//!
//! The rule: core `macros/mod.rs`, `include!`: "The file is located relative to the current
//! file (similarly to how modules are found)". The current file of the `include!` in
//! `data/mod.rs` is `data/mod.rs`, so it reads `data/a.rs.data`. Before the fix the host took
//! the path from the TOPMOST call's file (resolve.rs `expand_decls`: `Host { path: file.path,
//! .. }`, `file` the one of `src.loc`, and a call inside an expansion is placed at the call that
//! made it, task/lower.rs `Lowerer::loc`), so it asked for `src/a.rs.data`: missing, a
//! diagnostic and no items, and every macro the file exports is NotFound in the dependent.
//!
//! The control: crate `flat` includes the same `.rs.data` text in ONE step from `src/lib.rs`;
//! it resolves before and after the fix, so the miss is the nested include's path and not the
//! `.rs.data` extension, `#[macro_export]` in an expansion, or `pub use` of an exported macro by
//! its bare name.

use std::format;
use std::vec::Vec;

use blaze_surface::{Namespace, PathKind};

use super::entry::{Texts, input};
use crate::graph::Sysroot;
use crate::names::Written;
use crate::resolve::{Names, resolve_crates};
use crate::task::Serial;

const DATA_FILE: &str = "#[macro_export]\nmacro_rules! __m { () => {} }\npub use __m as m;\n";

const USER: &str = "#![no_core]
data::m!();
flat::m!();
pub mod glob {
    use data::*;
    m!();
}
";

static TEXTS: Texts = Texts(&[
    &[
        ("/data/src/lib.rs", "#![no_core]\n#[rustc_builtin_macro]\nmacro_rules! include { () => {} }\ninclude!(\"../data/mod.rs\");\n"),
        ("/data/data/mod.rs", "include!(\"a.rs.data\");\n"),
        ("/data/data/a.rs.data", DATA_FILE),
    ],
    &[("/flat/src/lib.rs", "#![no_core]\n#[rustc_builtin_macro]\nmacro_rules! include { () => {} }\ninclude!(\"../data/a.rs.data\");\n"), ("/flat/data/a.rs.data", DATA_FILE)],
    &[("/user/lib.rs", USER)],
]);

const DATA: usize = 0;
const FLAT: usize = 1;
const USER_CRATE: usize = 2;

fn solved() -> Names<'static> {
    let mut data = input("data", "/data/src/lib.rs", &[]);
    data.shown_from = "/data/";
    let mut flat = input("flat", "/flat/src/lib.rs", &[]);
    flat.shown_from = "/flat/";
    let mut user = input("user", "/user/lib.rs", &[("data", DATA), ("flat", FLAT)]);
    user.shown_from = "/user/";
    let crates = [data, flat, user];
    resolve_crates(&crates, Sysroot::default(), &TEXTS, &Serial, super::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"))
}

/// Whether crate `krate`'s root binds `name` in the macro namespace.
fn binds(names: &Names<'static>, krate: usize, name: &'static str) -> bool {
    let segments: &[&str] = &[name];
    names.item_at(names.root(krate), Namespace::Macros, Written { kind: PathKind::Plain, segments }, blaze_own::testing::heap().alloc()).is_ok()
}

/// Every scope error of crate `krate`, rendered.
fn errors(names: &Names<'static>, krate: usize) -> Vec<std::string::String> {
    names.krate(krate).done.values().flat_map(|done| done.scope.errors.iter().map(|e| format!("{e:?}"))).collect()
}

/// The control: one `include!` of the `.rs.data` file from `src/lib.rs` binds `__m` (the
/// `#[macro_export]` in the expansion) and `m` (the `pub use` by its bare name) at the root.
///
/// Passes today and after the fix: `tests::include_macros::a_one_step_include_of_rs_data_exports_its_macros ... ok`.
#[test]
fn a_one_step_include_of_rs_data_exports_its_macros() {
    let names = solved();
    assert!(binds(&names, FLAT, "__m"), "flat's root binds the exported `__m`; notes: {:?}", names.notes(FLAT));
    assert!(binds(&names, FLAT, "m"), "flat's root binds `m` by `pub use __m as m`; notes: {:?}", names.notes(FLAT));
}

/// The icu shape: `src/lib.rs` includes `data/mod.rs`, which includes `a.rs.data` relative to
/// ITSELF (`/data/data/a.rs.data`). The root binds `__m` and `m`, no note names a missing
/// include, and the user's `data::m!()` and glob-imported `m!()` resolve.
///
/// Fails before the fix with "data's root binds the exported `__m`" (the notes show an
/// `Include { .. Missing }` for `a.rs.data`: the host read `/data/src/a.rs.data`). After the
/// fix: `tests::include_macros::an_include_in_an_included_file_reads_relative_to_it ... ok`.
#[test]
fn an_include_in_an_included_file_reads_relative_to_it() {
    let names = solved();
    let notes = format!("{:?}", names.notes(DATA));
    assert!(!notes.contains("why: Missing"),"data: no include is missing: {notes}");
    assert!(binds(&names, DATA, "__m"), "data's root binds the exported `__m`; notes: {notes}");
    assert!(binds(&names, DATA, "m"), "data's root binds `m` by `pub use __m as m`; notes: {notes}");
    let user = errors(&names, USER_CRATE);
    assert!(user.is_empty(), "user: `data::m!()`, `flat::m!()` and the glob's `m!()` resolve: {user:?}\nnotes: {:?}", names.notes(USER_CRATE));
}
