//! The `#[macro_use]` prelude over the names records: a `#[macro_use] extern crate` imports the
//! macros the crate exports at its ROOT in the macro namespace, re-exports (`pub use`) included
//! (rustc's `process_macro_use_imports`: every macro child of the crate's root module). std's
//! `vec` is one: `pub use alloc_crate::vec;` (std lib.rs), and proptest (`#![no_std]`,
//! `#[macro_use] extern crate std;`) calls `vec![]` in `FileFailurePersistence`'s
//! `load_persisted_failures2`: tempo's sweep of 2026-11-21 printed it NOT RUN for "a path in
//! module 18b14d..: segment 0: not found" (names.rs `lexical` read only the root's
//! `#[macro_export]` rows; the names step's own layer, lookup.rs `lexical_layers`, read the
//! root's macro namespace).
//!
//! The crates: `k0` (alloc's place: `#[macro_export] macro_rules! vec`), `k1` (std's: `pub use
//! k0::vec;`, its own `#[macro_export] macro_rules! eprintln`, and `mod inner` with a
//! `macro_rules! hidden` and a `pub use k0::vec as deep`), `k2` (`#[macro_use] extern crate
//! k1;`, calling `vec!()` and `eprintln!()` at its root, which the names step resolves).
//!
//! 1. From `k2`'s root, `vec` names `k0`'s macro, from a fresh solve and from the records alike,
//!    and `eprintln` `k1`'s (the `#[macro_export]` rows, the layer before the fix).
//! 2. TWIN: `hidden` (a macro of a module under the root, no `#[macro_export]`) and `deep` (a
//!    re-export in that module) are no binding of `k1`'s root: not found.
//! 3. CONTROL, the cause removed: `k1` without its `pub use k0::vec;`: `vec` is not found.
//!
//! Expected lines: `test tests::names_macro_use::a_macro_use_crates_root_reexport_is_imported
//! ... ok`, `test tests::names_macro_use::control_without_the_reexport_vec_is_not_found ...
//! ok`. Before the fix, 1 failed at `vec` from the records: `Err(PathError { .., at: 0, why:
//! NotFound })`.

use std::format;
use std::vec::Vec;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_surface::{Namespace, PathKind};

use super::entry::{Texts, input};
use crate::crate_scope::{Ready, resolve_by_records};
use crate::graph::Sysroot;
use crate::names::{PathError, Resolved, Why, Written, path};
use crate::names_record::{LoadNames, RecordNames, encode};
use crate::resolve::{CrateNames, Names, aligned};
use crate::task::Serial;

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

const K0: &str = "#![no_core]
#[macro_export]
macro_rules! vec { () => {} }
";

const K1: &str = "#![no_core]
pub use k0::vec;
#[macro_export]
macro_rules! eprintln { () => {} }
pub mod inner {
    macro_rules! hidden { () => {} }
    pub use k0::vec as deep;
}
";

/// [`K1`] without the root's re-export of `vec`.
const K1_NO_REEXPORT: &str = "#![no_core]
#[macro_export]
macro_rules! eprintln { () => {} }
pub mod inner {
    macro_rules! hidden { () => {} }
    pub use k0::vec as deep;
}
";

const K2: &str = "#![no_core]
#[macro_use]
extern crate k1;
vec!();
eprintln!();
";

/// [`K2`] without the call of `vec!`, which [`K1_NO_REEXPORT`] does not give it.
const K2_NO_VEC: &str = "#![no_core]
#[macro_use]
extern crate k1;
eprintln!();
";

static TEXTS: Texts = Texts(&[&[("/k0/lib.rs", K0)], &[("/k1/lib.rs", K1)], &[("/k2/lib.rs", K2)]]);
static CONTROL: Texts = Texts(&[&[("/k0/lib.rs", K0)], &[("/k1/lib.rs", K1_NO_REEXPORT)], &[("/k2/lib.rs", K2_NO_VEC)]]);

struct Loaded(Vec<&'static [u8]>);

impl LoadNames<'static> for Loaded {
    fn load(&self, krate: usize) -> Option<&'static [u8]> {
        self.0.get(krate).copied()
    }
}

/// The three crates of `texts` solved: the fresh solve's names and the records' reader, and
/// each crate's root module.
fn solved(texts: &'static Texts) -> (Names<'static>, RecordNames<'static, Loaded>, [ItemKey; 3]) {
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("k0", "/k0/lib.rs", &[]), input("k1", "/k1/lib.rs", &[("k0", 0)]), input("k2", "/k2/lib.rs", &[("k1", 1)])];
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), texts, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("solve: {e:?}"));
    let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
    for (i, c) in all.iter().enumerate() {
        // The names step resolved `k2`'s root calls through its own `#[macro_use]` layer.
        assert!(c.notes.is_empty(), "crate k{i}: notes {:?}", c.notes);
    }
    let fresh = Names::of_crates(&inputs, Sysroot::default(), &all, g).unwrap_or_else(|e| panic!("names: {e:?}"));
    let records: Vec<&'static [u8]> = all.iter().map(|n| aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump)).collect();
    let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
    let read = RecordNames::new(&keys, Loaded(records), g);
    let roots = [fresh.root(0), fresh.root(1), fresh.root(2)];
    (fresh, read, roots)
}

/// `name` as a one-segment macro path from `module`: from the fresh solve and from the records.
fn macro_at(fresh: &Names<'static>, read: &RecordNames<'static, Loaded>, module: ItemKey, name: &'static str) -> [Result<Resolved, PathError>; 2] {
    let g = blaze_own::testing::heap().alloc();
    let segments = [name];
    let written = Written { kind: PathKind::Plain, segments: &segments[..] };
    [path(fresh, module, Namespace::Macros, written, g), path(read, module, Namespace::Macros, written, g)]
}

fn not_found(r: &Result<Resolved, PathError>) -> bool {
    matches!(r, Err(PathError { why: Why::NotFound, .. }))
}

/// 1 and 2: the root's re-export is imported, the root's own export too; a module's macro
/// and a module's re-export are not.
#[test]
fn a_macro_use_crates_root_reexport_is_imported() {
    let (fresh, read, [k0, k1, k2]) = solved(&TEXTS);
    let mut wrong = Vec::new();
    for (name, home) in [("vec", k0), ("eprintln", k1)] {
        // What the name is where it is written: its crate's own textual scope.
        let [want, _] = macro_at(&fresh, &read, home, name);
        let want = want.unwrap_or_else(|e| panic!("`{name}` in its own crate: {e:?}"));
        for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, k2, name)) {
            if got.as_ref().map(|r| r.def) != Ok(want.def) {
                wrong.push(format!("`{name}` from k2's root ({side}): {got:?}, want {:?}", want.def));
            }
        }
    }
    for name in ["hidden", "deep"] {
        for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, k2, name)) {
            if !not_found(&got) {
                wrong.push(format!("TWIN `{name}` (k1's `inner`, not its root) from k2's root ({side}): {got:?}, want not found"));
            }
        }
    }
    assert!(wrong.is_empty(), "{wrong:?}");
}

/// 3. CONTROL: without `k1`'s `pub use k0::vec;` nothing imports `vec`, and `eprintln` still is.
#[test]
fn control_without_the_reexport_vec_is_not_found() {
    let (fresh, read, [_, k1, k2]) = solved(&CONTROL);
    for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, k2, "vec")) {
        assert!(not_found(&got), "`vec` from k2's root ({side}) with no re-export at k1's root: {got:?}");
    }
    let [want, _] = macro_at(&fresh, &read, k1, "eprintln");
    let want = want.unwrap_or_else(|e| panic!("`eprintln` in k1: {e:?}"));
    for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, k2, "eprintln")) {
        assert_eq!(got.map(|r| r.def), Ok(want.def), "`eprintln` from k2's root ({side})");
    }
}

/// An out-of-line module file with inner `#![macro_use]` exports its `macro_rules!` to subsequent
/// sibling modules, and a `#[cfg_attr(..., // comment, \n allow(...))]` comment with commas does
/// not split into bogus attribute macros.
#[test]
fn inner_macro_use_on_out_of_line_module_and_cfg_attr_comments() {
    static LIB: &str = "#![no_core]
mod defs;
mod user;
#[cfg_attr(
    not(feature = \"nope\"),
    // NB: Rust 1.89, current stable, has commas here.
    // however, does, allow below still parses cleanly.
    allow(dead_code, reason = \"keep\")
)]
pub trait Kept {}
";
    static DEFS: &str = "#![macro_use]
macro_rules! make_type {
    ($name:ident) => { pub struct $name; };
}
";
    static USER: &str = "make_type!(Made);
pub type Alias = (Made, crate::Kept);
";
    static FILES: Texts = Texts(&[&[("/k/lib.rs", LIB), ("/k/defs.rs", DEFS), ("/k/user.rs", USER)]]);
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("k", "/k/lib.rs", &[])];
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), &FILES, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("solve: {e:?}"));
    let c = solved[0].names;
    assert!(c.notes.is_empty(), "expected no resolution notes, got {:?}", c.notes);
    let fresh = Names::of_crates(&inputs, Sysroot::default(), &[c], g).unwrap_or_else(|e| panic!("names: {e:?}"));
    let root = fresh.root(0);
    let kept = Written { kind: PathKind::Plain, segments: &["Kept"] };
    assert!(path(&fresh, root, Namespace::Types, kept, g).is_ok(), "`Kept` must be declared despite comments in #[cfg_attr]");
    let made = Written { kind: PathKind::Plain, segments: &["user", "Made"] };
    assert!(path(&fresh, root, Namespace::Types, made, g).is_ok(), "`user::Made` must expand via inner #![macro_use] in defs.rs");
}

/// End-to-end C2-A & C2-B integration test over full lowering + `resolve_by_records` + both
/// fresh `Names` and encoded `RecordNames`:
/// - C2-B: `pub macro assert_matches` re-exported at `std_like` crate root is excluded from
///   `#[macro_use] extern crate std_like`, so `use other::assert_matches;` + `use super::*;`
///   resolves cleanly, while explicit `std_like::assert_matches!()` and `use std_like::assert_matches;`
///   still work, `pub macro cfg_select` in `std_like::prelude::v1` still resolves via prelude, and
///   `#[macro_export] macro_rules! legacy_mac` at `std_like` root still enters `#[macro_use]` prelude.
/// - C2-A: named `use other::assert_eq;` in a module + unrelated enum/module glob in a function
///   body resolves `assert_eq!()` cleanly without `GlobVsOuter`.
#[test]
fn macro2_excluded_from_macro_use_prelude_and_block_glob_c2_end_to_end() {
    static CORE_LIKE: &str = "#![no_core]
pub macro assert_matches {
    () => {}
}
pub macro cfg_select($($t:tt)*) {
    $($t)*
}
#[macro_export]
macro_rules! legacy_mac {
    () => {}
}
#[macro_export]
macro_rules! assert_eq {
    () => {}
}
pub mod prelude {
    pub mod v1 {
        pub use crate::cfg_select;
    }
}
";
    static STD_LIKE: &str = "#![no_core]
pub use core_like::{assert_matches, assert_eq, legacy_mac};
pub mod prelude {
    pub mod v1 {
        pub use core_like::prelude::v1::cfg_select;
    }
}
";
    static OTHER: &str = "#![no_core]
#[macro_export]
macro_rules! assert_matches {
    () => { pub struct FromOtherAssertMatches; }
}
#[macro_export]
macro_rules! assert_eq {
    () => { pub struct FromOtherAssertEq; }
}
#[macro_export]
macro_rules! legacy_mac {
    () => {}
}
";
    static USER: &str = "#![no_core]
#[macro_use]
extern crate std_like;
extern crate other;

pub mod parent_ok {
    use other::assert_matches;
    pub mod child {
        use super::*;
        assert_matches!();
    }
}

pub mod explicit_macro2 {
    use std_like::assert_matches;
    assert_matches!();
    pub mod direct {
        std_like::assert_matches!();
    }
}

pub mod via_prelude {
    use std_like::prelude::v1::*;
    cfg_select! { pub struct FromCfgSelect; }
}

pub enum MyEnum {
    A,
}

pub mod unrelated {
    pub struct Unused;
}

pub mod block_with_glob {
    use other::assert_eq;
    pub fn check() {
        use crate::MyEnum::*;
        use crate::unrelated::*;
        assert_eq!();
    }
}
";
    static USER_CONTROL_LEGACY: &str = "#![no_core]
#[macro_use]
extern crate std_like;
extern crate other;

pub mod parent_bad {
    use other::legacy_mac;
    pub mod child {
        use super::*;
        legacy_mac!();
    }
}
";
    static FILES: Texts = Texts(&[
        &[("/core_like/lib.rs", CORE_LIKE)],
        &[("/std_like/lib.rs", STD_LIKE)],
        &[("/other/lib.rs", OTHER)],
        &[("/user/lib.rs", USER)],
    ]);
    static CONTROL_FILES: Texts = Texts(&[
        &[("/core_like/lib.rs", CORE_LIKE)],
        &[("/std_like/lib.rs", STD_LIKE)],
        &[("/other/lib.rs", OTHER)],
        &[("/user/lib.rs", USER_CONTROL_LEGACY)],
    ]);

    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [
        input("core_like", "/core_like/lib.rs", &[]),
        input("std_like", "/std_like/lib.rs", &[("core_like", 0)]),
        input("other", "/other/lib.rs", &[]),
        input("user", "/user/lib.rs", &[("std_like", 1), ("other", 2)]),
    ];
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), &FILES, &Serial, &Ready::serial(WAIT), bump)
        .unwrap_or_else(|e| panic!("solve: {e:?}"));
    let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
    for (i, c) in all.iter().enumerate() {
        assert!(c.notes.is_empty(), "crate {i} had unexpected notes: {:?}", c.notes);
        for (m, done) in c.done.iter() {
            assert!(done.scope.errors.is_empty(), "crate {i} module {m:x} had unexpected scope errors: {:?}", done.scope.errors);
        }
    }

    let fresh = Names::of_crates(&inputs, Sysroot::default(), &all, g).unwrap_or_else(|e| panic!("names: {e:?}"));
    let records: Vec<&'static [u8]> = all.iter().map(|n| aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump)).collect();
    let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
    let read = RecordNames::new(&keys, Loaded(records), g);
    let user_root = fresh.root(3);

    // 1. `assert_matches` at user root is NOT in #[macro_use] prelude (both fresh and records).
    for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, user_root, "assert_matches")) {
        assert!(not_found(&got), "`assert_matches` (pub macro) must not be in #[macro_use] prelude ({side}): {got:?}");
    }
    // 2. `legacy_mac` at user root IS in #[macro_use] prelude (both fresh and records).
    for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, user_root, "legacy_mac")) {
        assert!(got.is_ok(), "`legacy_mac` (#[macro_export] macro_rules!) must be in #[macro_use] prelude ({side}): {got:?}");
    }
    // 3. Explicit path `std_like::assert_matches` resolves cleanly (both fresh and records).
    let explicit_w = Written { kind: PathKind::Plain, segments: &["std_like", "assert_matches"] };
    assert!(path(&fresh, user_root, Namespace::Macros, explicit_w, g).is_ok(), "fresh explicit std_like::assert_matches");
    assert!(path(&read, user_root, Namespace::Macros, explicit_w, g).is_ok(), "records explicit std_like::assert_matches");
    // 4. Expansion outputs from `parent_ok::child` and `via_prelude` exist.
    let from_other = Written { kind: PathKind::Plain, segments: &["parent_ok", "child", "FromOtherAssertMatches"] };
    assert!(path(&fresh, user_root, Namespace::Types, from_other, g).is_ok(), "other::assert_matches expanded in child");
    let from_cfg = Written { kind: PathKind::Plain, segments: &["via_prelude", "FromCfgSelect"] };
    assert!(path(&fresh, user_root, Namespace::Types, from_cfg, g).is_ok(), "cfg_select expanded via prelude");

    // 5. Control: `legacy_mac` (legacy #[macro_export]) STILL causes GlobVsOuter in child!
    let (ctrl_solved, _) = resolve_by_records(&inputs, Sysroot::default(), &CONTROL_FILES, &Serial, &Ready::serial(WAIT), bump)
        .unwrap_or_else(|e| panic!("control solve: {e:?}"));
    let has_glob_vs_outer = ctrl_solved[3].names.done.values().any(|done| {
        done.scope.errors.iter().any(|err| {
            matches!(
                err,
                blaze_surface::ScopeError::Macro {
                    why: blaze_surface::Unresolved::Ambiguous(blaze_surface::AmbiguityKind::GlobVsOuter),
                    ..
                }
            )
        })
    });
    assert!(
        has_glob_vs_outer,
        "control: legacy #[macro_export] macro_rules! in #[macro_use] prelude must still conflict with `use super::*;`"
    );
}

/// Macro-expanded `#[macro_use] extern crate dep;` at the crate root imports `dep`'s exported
/// macros into the `#[macro_use]` prelude for both the crate root (including calls inside the
/// same expansion and siblings before/after it) and child modules, in fresh `Names` and encoded
/// `RecordNames` alike (reqwest's `if_hyper! { #[macro_use] extern crate doc_comment; doctest!(..); }`).
#[test]
fn macro_expanded_macro_use_extern_crate_imports_macros() {
    static DEP: &str = "#![no_core]
pub struct DepType;
#[macro_export]
macro_rules! dep_macro {
    ($name:ident) => { pub struct $name; };
}
";
    static USER: &str = "#![no_core]
macro_rules! if_hyper {
    ($($item:item)*) => { $($item)* };
}
pub mod before {
    dep_macro!(FromBefore);
}
if_hyper! {
    #[macro_use]
    extern crate dep;
    dep_macro!(FromExpansion);
}
dep_macro!(FromRoot);
pub mod after {
    dep_macro!(FromAfter);
}
";
    static FILES: Texts = Texts(&[&[("/dep/lib.rs", DEP)], &[("/user/lib.rs", USER)]]);
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("dep", "/dep/lib.rs", &[]), input("user", "/user/lib.rs", &[("dep", 0)])];
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), &FILES, &Serial, &Ready::serial(WAIT), bump)
        .unwrap_or_else(|e| panic!("solve: {e:?}"));
    let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
    for (i, c) in all.iter().enumerate() {
        assert!(c.notes.is_empty(), "crate {i}: unexpected notes {:?}", c.notes);
        for (m, done) in c.done.iter() {
            assert!(done.scope.errors.is_empty(), "crate {i} module {m:x} had unexpected errors: {:?}", done.scope.errors);
        }
    }
    let fresh = Names::of_crates(&inputs, Sysroot::default(), &all, g).unwrap_or_else(|e| panic!("names: {e:?}"));
    let records: Vec<&'static [u8]> = all.iter().map(|n| aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump)).collect();
    let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
    let read = RecordNames::new(&keys, Loaded(records), g);
    let dep_root = fresh.root(0);
    let user_root = fresh.root(1);
    let [want_mac, _] = macro_at(&fresh, &read, dep_root, "dep_macro");
    let want_mac = want_mac.unwrap_or_else(|e| panic!("dep_macro in dep: {e:?}"));

    for (label, segments) in [
        ("FromExpansion", &["FromExpansion"][..]),
        ("FromRoot", &["FromRoot"][..]),
        ("before::FromBefore", &["before", "FromBefore"][..]),
        ("after::FromAfter", &["after", "FromAfter"][..]),
    ] {
        let w = Written { kind: PathKind::Plain, segments };
        let a = path(&fresh, user_root, Namespace::Types, w, g);
        let b = path(&read, user_root, Namespace::Types, w, g);
        assert!(a.is_ok() && a == b, "`{label}`: fresh {a:?}, records {b:?}");
    }
    let before_mod = path(&fresh, user_root, Namespace::Types, Written { kind: PathKind::Plain, segments: &["before"] }, g)
        .unwrap_or_else(|e| panic!("before mod: {e:?}"))
        .def
        .key;
    let after_mod = path(&fresh, user_root, Namespace::Types, Written { kind: PathKind::Plain, segments: &["after"] }, g)
        .unwrap_or_else(|e| panic!("after mod: {e:?}"))
        .def
        .key;
    for (place, module) in [("root", user_root), ("before", before_mod), ("after", after_mod)] {
        for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, module, "dep_macro")) {
            assert_eq!(got.map(|r| r.def), Ok(want_mac.def), "`dep_macro` in {place} ({side})");
        }
    }
}

/// Macro-expanded `#[macro_use] extern crate dep as _;` and `#[macro_use] extern crate dep as alias;`
/// both import `dep`'s exported macros; `as _` adds no type-namespace binding in the root while
/// `as alias` binds `alias` in both the root and the extern prelude.
#[test]
fn macro_expanded_macro_use_extern_crate_as_underscore_and_alias() {
    static DEP: &str = "#![no_core]
pub struct DepType;
#[macro_export]
macro_rules! dep_macro {
    ($name:ident) => { pub struct $name; };
}
";
    static UNDERSCORE: &str = "#![no_core]
macro_rules! if_hyper {
    ($($item:item)*) => { $($item)* };
}
if_hyper! {
    #[macro_use]
    extern crate dep as _;
    dep_macro!(FromUnderscore);
}
pub mod sub {
    dep_macro!(FromSub);
}
";
    static ALIAS: &str = "#![no_core]
macro_rules! if_hyper {
    ($($item:item)*) => { $($item)* };
}
if_hyper! {
    #[macro_use]
    extern crate dep as alias;
    dep_macro!(FromAlias);
}
pub use ::alias::DepType as Reexported;
";
    static FILES_UNDERSCORE: Texts = Texts(&[&[("/dep/lib.rs", DEP)], &[("/user/lib.rs", UNDERSCORE)]]);
    static FILES_ALIAS: Texts = Texts(&[&[("/dep/lib.rs", DEP)], &[("/user/lib.rs", ALIAS)]]);

    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("dep", "/dep/lib.rs", &[]), input("user", "/user/lib.rs", &[("dep", 0)])];

    // 1. `#[macro_use] extern crate dep as _;`
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), &FILES_UNDERSCORE, &Serial, &Ready::serial(WAIT), bump)
        .unwrap_or_else(|e| panic!("underscore solve: {e:?}"));
    let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
    for (i, c) in all.iter().enumerate() {
        assert!(c.notes.is_empty(), "underscore crate {i}: notes {:?}", c.notes);
        for (m, done) in c.done.iter() {
            assert!(done.scope.errors.is_empty(), "underscore crate {i} module {m:x} errors: {:?}", done.scope.errors);
        }
    }
    let fresh = Names::of_crates(&inputs, Sysroot::default(), &all, g).unwrap_or_else(|e| panic!("names: {e:?}"));
    let records: Vec<&'static [u8]> = all.iter().map(|n| aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump)).collect();
    let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
    let read = RecordNames::new(&keys, Loaded(records), g);
    let user_root = fresh.root(1);
    for segments in [&["FromUnderscore"][..], &["sub", "FromSub"][..]] {
        let w = Written { kind: PathKind::Plain, segments };
        let a = path(&fresh, user_root, Namespace::Types, w, g);
        let b = path(&read, user_root, Namespace::Types, w, g);
        assert!(a.is_ok() && a == b, "underscore {segments:?}: fresh {a:?}, records {b:?}");
    }
    for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, user_root, "dep_macro")) {
        assert!(got.is_ok(), "underscore `dep_macro` ({side}): {got:?}");
    }
    // `as _` binds nothing in the module's own scope (`self::dep` is not found).
    let self_dep = Written { kind: PathKind::SelfMod, segments: &["dep"] };
    assert!(not_found(&path(&fresh, user_root, Namespace::Types, self_dep, g)), "`self::dep` must not be bound by `as _`");

    // 2. `#[macro_use] extern crate dep as alias;`
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), &FILES_ALIAS, &Serial, &Ready::serial(WAIT), bump)
        .unwrap_or_else(|e| panic!("alias solve: {e:?}"));
    let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
    for (i, c) in all.iter().enumerate() {
        assert!(c.notes.is_empty(), "alias crate {i}: notes {:?}", c.notes);
        for (m, done) in c.done.iter() {
            assert!(done.scope.errors.is_empty(), "alias crate {i} module {m:x} errors: {:?}", done.scope.errors);
        }
    }
    let fresh = Names::of_crates(&inputs, Sysroot::default(), &all, g).unwrap_or_else(|e| panic!("names: {e:?}"));
    let records: Vec<&'static [u8]> = all.iter().map(|n| aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump)).collect();
    let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
    let read = RecordNames::new(&keys, Loaded(records), g);
    let user_root = fresh.root(1);
    for segments in [&["FromAlias"][..], &["Reexported"][..]] {
        let w = Written { kind: PathKind::Plain, segments };
        let a = path(&fresh, user_root, Namespace::Types, w, g);
        let b = path(&read, user_root, Namespace::Types, w, g);
        assert!(a.is_ok() && a == b, "alias {segments:?}: fresh {a:?}, records {b:?}");
    }
    for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, user_root, "dep_macro")) {
        assert!(got.is_ok(), "alias `dep_macro` ({side}): {got:?}");
    }
}

/// Negative controls for macro-expanded `extern crate`:
/// 1. Without `#[macro_use]`, `dep`'s macros are NOT imported.
/// 2. Under `#[cfg(false)]`, `#[macro_use] extern crate dep;` is stripped and imports nothing.
/// 3. In a non-root module, `#[macro_use] extern crate dep;` does NOT enter the `#[macro_use]` prelude.
#[test]
fn macro_expanded_extern_crate_negative_controls() {
    static DEP: &str = "#![no_core]
#[macro_export]
macro_rules! dep_macro {
    ($name:ident) => { pub struct $name; };
}
";
    static NO_MACRO_USE: &str = "#![no_core]
macro_rules! if_hyper {
    ($($item:item)*) => { $($item)* };
}
if_hyper! {
    extern crate dep;
    dep_macro!(Nope);
}
";
    static CFG_FALSE: &str = "#![no_core]
macro_rules! if_hyper {
    ($($item:item)*) => { $($item)* };
}
if_hyper! {
    #[cfg(false)]
    #[macro_use]
    extern crate dep;
    dep_macro!(Nope);
}
";
    static IN_SUBMOD: &str = "#![no_core]
#[macro_use]
mod macros {
    macro_rules! if_hyper {
        ($($item:item)*) => { $($item)* };
    }
}
pub mod inner {
    if_hyper! {
        #[macro_use]
        extern crate dep;
        dep_macro!(Nope);
    }
}
";
    static FILES_NO_ATTR: Texts = Texts(&[&[("/dep/lib.rs", DEP)], &[("/user/lib.rs", NO_MACRO_USE)]]);
    static FILES_CFG_FALSE: Texts = Texts(&[&[("/dep/lib.rs", DEP)], &[("/user/lib.rs", CFG_FALSE)]]);
    static FILES_SUBMOD: Texts = Texts(&[&[("/dep/lib.rs", DEP)], &[("/user/lib.rs", IN_SUBMOD)]]);

    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("dep", "/dep/lib.rs", &[]), input("user", "/user/lib.rs", &[("dep", 0)])];

    for (label, files) in [
        ("without #[macro_use]", &FILES_NO_ATTR),
        ("#[cfg(false)] #[macro_use]", &FILES_CFG_FALSE),
        ("non-root #[macro_use]", &FILES_SUBMOD),
    ] {
        let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), files, &Serial, &Ready::serial(WAIT), bump)
            .unwrap_or_else(|e| panic!("{label}: solve failed: {e:?}"));
        let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
        let has_unresolved = all[1].done.values().any(|done| {
            done.scope.errors.iter().any(|err| {
                matches!(err, blaze_surface::ScopeError::Macro { why: blaze_surface::Unresolved::NotFound, .. })
            })
        });
        assert!(has_unresolved, "{label}: expected unresolved macro error for `dep_macro!`, got done={:?}", all[1].done.values().map(|d| &d.scope.errors).collect::<Vec<_>>());
        let fresh = Names::of_crates(&inputs, Sysroot::default(), &all, g).unwrap_or_else(|e| panic!("names: {e:?}"));
        let records: Vec<&'static [u8]> = all.iter().map(|n| aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump)).collect();
        let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
        let read = RecordNames::new(&keys, Loaded(records), g);
        let user_root = fresh.root(1);
        for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, user_root, "dep_macro")) {
            assert!(not_found(&got), "{label}: `dep_macro` must not be in #[macro_use] prelude ({side}): {got:?}");
        }
    }
}

/// Selective `#[macro_use(allowed)] extern crate dep;` (both macro-expanded and empty `#[macro_use()]`)
/// imports only the listed macros in both fresh `Names` and encoded `RecordNames` / child modules.
#[test]
fn macro_expanded_selective_macro_use_allowlist() {
    static DEP: &str = "#![no_core]
#[macro_export]
macro_rules! allowed {
    ($name:ident) => { pub struct $name; };
}
#[macro_export]
macro_rules! excluded {
    ($name:ident) => { pub struct $name; };
}
";
    static SELECTIVE: &str = "#![no_core]
macro_rules! if_hyper {
    ($($item:item)*) => { $($item)* };
}
if_hyper! {
    #[macro_use(allowed)]
    extern crate dep;
    allowed!(FromExpansion);
}
pub mod sub {
    allowed!(FromSub);
}
";
    static SELECTIVE_EXCLUDED: &str = "#![no_core]
macro_rules! if_hyper {
    ($($item:item)*) => { $($item)* };
}
if_hyper! {
    #[macro_use(allowed)]
    extern crate dep;
    excluded!(Nope);
}
";
    static EMPTY_ALLOWLIST: &str = "#![no_core]
macro_rules! if_hyper {
    ($($item:item)*) => { $($item)* };
}
if_hyper! {
    #[macro_use()]
    extern crate dep;
    allowed!(Nope);
}
";
    static FILES_OK: Texts = Texts(&[&[("/dep/lib.rs", DEP)], &[("/user/lib.rs", SELECTIVE)]]);
    static FILES_EXCLUDED: Texts = Texts(&[&[("/dep/lib.rs", DEP)], &[("/user/lib.rs", SELECTIVE_EXCLUDED)]]);
    static FILES_EMPTY: Texts = Texts(&[&[("/dep/lib.rs", DEP)], &[("/user/lib.rs", EMPTY_ALLOWLIST)]]);

    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("dep", "/dep/lib.rs", &[]), input("user", "/user/lib.rs", &[("dep", 0)])];

    // 1. Positive: `allowed!` resolves in root, expansion, and child module; `excluded` is NotFound in prelude.
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), &FILES_OK, &Serial, &Ready::serial(WAIT), bump)
        .unwrap_or_else(|e| panic!("selective solve: {e:?}"));
    let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
    for (i, c) in all.iter().enumerate() {
        assert!(c.notes.is_empty(), "crate {i}: notes {:?}", c.notes);
        for (m, done) in c.done.iter() {
            assert!(done.scope.errors.is_empty(), "crate {i} module {m:x} errors: {:?}", done.scope.errors);
        }
    }
    let fresh = Names::of_crates(&inputs, Sysroot::default(), &all, g).unwrap_or_else(|e| panic!("names: {e:?}"));
    let records: Vec<&'static [u8]> = all.iter().map(|n| aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump)).collect();
    let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
    let read = RecordNames::new(&keys, Loaded(records), g);
    let user_root = fresh.root(1);
    for segments in [&["FromExpansion"][..], &["sub", "FromSub"][..]] {
        let w = Written { kind: PathKind::Plain, segments };
        let a = path(&fresh, user_root, Namespace::Types, w, g);
        let b = path(&read, user_root, Namespace::Types, w, g);
        assert!(a.is_ok() && a == b, "selective {segments:?}: fresh {a:?}, records {b:?}");
    }
    for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, user_root, "allowed")) {
        assert!(got.is_ok(), "`allowed` must be in #[macro_use] prelude ({side}): {got:?}");
    }
    for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, user_root, "excluded")) {
        assert!(not_found(&got), "`excluded` must NOT be in #[macro_use] prelude ({side}): {got:?}");
    }

    // 2. Negative: calling `excluded!()` fails with Unresolved::NotFound, and `#[macro_use()]` imports nothing.
    for (label, files, mac) in [
        ("excluded macro", &FILES_EXCLUDED, "excluded"),
        ("empty #[macro_use()]", &FILES_EMPTY, "allowed"),
    ] {
        let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), files, &Serial, &Ready::serial(WAIT), bump)
            .unwrap_or_else(|e| panic!("{label}: solve failed: {e:?}"));
        let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
        let has_unresolved = all[1].done.values().any(|done| {
            done.scope.errors.iter().any(|err| {
                matches!(err, blaze_surface::ScopeError::Macro { why: blaze_surface::Unresolved::NotFound, .. })
            })
        });
        assert!(has_unresolved, "{label}: expected Unresolved::NotFound for `{mac}!`");
    }
}

/// Canonical expanded source order:
/// 1. `emit_dep1!(); #[macro_use] extern crate dep2;` -> order `[dep1, dep2]`, so `dep2::dup!` shadows `dep1::dup!`.
/// 2. `emit_dep1!(); emit_dep2!();` -> order `[dep1, dep2]`, so `dep2::dup!` shadows `dep1::dup!`.
/// Verified on both fresh `Names` and encoded `RecordNames`!
#[test]
fn macro_expanded_macro_use_preserves_canonical_source_order() {
    static DEP1: &str = "#![no_core]
#[macro_export]
macro_rules! dup {
    () => { pub struct FromDep1; };
}
";
    static DEP2: &str = "#![no_core]
#[macro_export]
macro_rules! dup {
    () => { pub struct FromDep2; };
}
";
    static MIXED: &str = "#![no_core]
macro_rules! emit_dep1 {
    () => { #[macro_use] extern crate dep1; };
}
emit_dep1!();
#[macro_use]
extern crate dep2;
pub mod sub {
    dup!();
}
";
    static BOTH_EXPANDED: &str = "#![no_core]
macro_rules! emit_dep1 {
    () => { #[macro_use] extern crate dep1; };
}
macro_rules! emit_dep2 {
    () => { #[macro_use] extern crate dep2; };
}
emit_dep1!();
emit_dep2!();
pub mod sub {
    dup!();
}
";
    static FILES_MIXED: Texts = Texts(&[&[("/dep1/lib.rs", DEP1)], &[("/dep2/lib.rs", DEP2)], &[("/user/lib.rs", MIXED)]]);
    static FILES_BOTH: Texts = Texts(&[&[("/dep1/lib.rs", DEP1)], &[("/dep2/lib.rs", DEP2)], &[("/user/lib.rs", BOTH_EXPANDED)]]);

    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [
        input("dep1", "/dep1/lib.rs", &[]),
        input("dep2", "/dep2/lib.rs", &[]),
        input("user", "/user/lib.rs", &[("dep1", 0), ("dep2", 1)]),
    ];
    for (label, files) in [("expanded-before-source", &FILES_MIXED), ("two-expanded", &FILES_BOTH)] {
        let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), files, &Serial, &Ready::serial(WAIT), bump)
            .unwrap_or_else(|e| panic!("{label}: solve: {e:?}"));
        let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
        for (i, c) in all.iter().enumerate() {
            assert!(c.notes.is_empty(), "{label} crate {i}: notes {:?}", c.notes);
            for (m, done) in c.done.iter() {
                assert!(done.scope.errors.is_empty(), "{label} crate {i} module {m:x} errors: {:?}", done.scope.errors);
            }
        }
        let fresh = Names::of_crates(&inputs, Sysroot::default(), &all, g).unwrap_or_else(|e| panic!("names: {e:?}"));
        let records: Vec<&'static [u8]> = all.iter().map(|n| aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump)).collect();
        let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
        let read = RecordNames::new(&keys, Loaded(records), g);
        let dep2_root = fresh.root(1);
        let user_root = fresh.root(2);
        let [want_dup, _] = macro_at(&fresh, &read, dep2_root, "dup");
        let want_dup = want_dup.unwrap_or_else(|e| panic!("dep2::dup: {e:?}"));
        for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, user_root, "dup")) {
            assert_eq!(got.map(|r| r.def), Ok(want_dup.def), "{label}: `dup` ({side}) must resolve to dep2::dup");
        }
        let from_dep2 = Written { kind: PathKind::Plain, segments: &["sub", "FromDep2"] };
        let from_dep1 = Written { kind: PathKind::Plain, segments: &["sub", "FromDep1"] };
        assert!(path(&fresh, user_root, Namespace::Types, from_dep2, g).is_ok(), "{label}: sub::FromDep2 must exist");
        assert!(path(&read, user_root, Namespace::Types, from_dep2, g).is_ok(), "{label}: sub::FromDep2 must exist in records");
        assert!(not_found(&path(&fresh, user_root, Namespace::Types, from_dep1, g)), "{label}: sub::FromDep1 must not exist");
    }
}

/// Incremental one-group solve (`tree_record` updated with post-expansion `CrateRoot`):
/// editing ONLY a non-root module `mod sub;` still resolves macros from an unchanged root
/// macro-expanded `#[macro_use] extern crate dep;` via `tree_record`'s embedded crate root.
#[test]
fn one_group_solve_after_child_edit_keeps_root_macro_expanded_macro_use() {
    use crate::cells::{GroupRecords, GroupRef, Members};
    use crate::crate_scope::resolve_members;
    use crate::task::Par;

    struct WithGroups;
    impl Par for WithGroups {
        fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut blaze_own::AVec<'_, T>) {
            Serial.map(n, f, out);
        }
        fn names_groups(&self) -> bool {
            true
        }
    }

    struct MapRecords<'a>(&'a [crate::NameGroup<'a>]);
    impl GroupRecords for MapRecords<'_> {
        fn group_of(&self, module: ItemKey) -> Option<GroupRef<'_>> {
            self.0.iter().find(|g| g.members.binary_search(&module).is_ok()).map(GroupRef::of)
        }
    }

    static DEP: &str = "#![no_core]
#[macro_export]
macro_rules! dep_macro {
    ($name:ident) => { pub struct $name; };
}
";
    static USER_ROOT: &str = "#![no_core]
macro_rules! if_hyper {
    ($($item:item)*) => { $($item)* };
}
if_hyper! {
    #[macro_use]
    extern crate dep;
}
pub mod sub;
";
    static SUB_BEFORE: &str = "dep_macro!(FromSub);
";
    static SUB_AFTER: &str = "dep_macro!(FromSub);
dep_macro!(FromEditedSub);
";
    static FILES_BEFORE: Texts = Texts(&[&[("/dep/lib.rs", DEP)], &[("/user/lib.rs", USER_ROOT), ("/user/sub.rs", SUB_BEFORE)]]);
    static FILES_AFTER: Texts = Texts(&[&[("/dep/lib.rs", DEP)], &[("/user/lib.rs", USER_ROOT), ("/user/sub.rs", SUB_AFTER)]]);

    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let dep_in = input("dep", "/dep/lib.rs", &[]);
    let user_in = input("user", "/user/lib.rs", &[("dep", 0)]);
    let inputs = [dep_in, user_in];

    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), &FILES_BEFORE, &WithGroups, &Ready::serial(WAIT), bump)
        .unwrap_or_else(|e| panic!("initial solve: {e:?}"));
    let dep_record = aligned(&solved[0].record, bump);
    let user_names = solved[1].names;
    assert!(!user_names.tree_record.is_empty(), "expected tree_record to be populated");
    let fresh = Names::of_crates(&inputs, Sysroot::default(), &[solved[0].names, user_names], g)
        .unwrap_or_else(|e| panic!("names: {e:?}"));
    let sub_key = path(&fresh, fresh.root(1), Namespace::Types, Written { kind: PathKind::Plain, segments: &["sub"] }, g)
        .unwrap_or_else(|e| panic!("sub key: {e:?}"))
        .def
        .key;

    let records = MapRecords(&user_names.groups);
    let deps_built: &[&[u8]] = &[dep_record];
    let member_list = [sub_key];
    let members = Members { members: &member_list, records: &records, tree: Some(user_names.tree_record) };
    let one = resolve_members(&user_in, deps_built, Sysroot::default(), &FILES_AFTER, 1, &Serial, members, bump)
        .unwrap_or_else(|e| panic!("one-group solve of edited `sub`: {e:?}"));
    let sub_group = one.groups.iter().find(|grp| grp.members.binary_search(&sub_key).is_ok())
        .unwrap_or_else(|| panic!("missing group for edited `sub`"));
    let scope_bytes = GroupRef::of(sub_group).scope_of(sub_key).unwrap_or_else(|e| panic!("scope_of: {e:?}"));
    let view = blaze_surface::ScopeView::read(scope_bytes).unwrap_or_else(|e| panic!("ScopeView::read: {e:?}"));
    assert!(view.errors().next().is_none(), "edited `sub` had unexpected scope errors: {:?}", view.errors().collect::<Vec<_>>());
    assert!(view.get(Namespace::Types, "FromSub").is_some(), "`FromSub` must resolve in one-group solve");
    assert!(view.get(Namespace::Types, "FromEditedSub").is_some(), "`FromEditedSub` must resolve in one-group solve");
}

/// Full Unicode Rust identifiers (and raw identifiers) in `#[macro_use(π, café)] extern crate dep;`:
/// `π!()` and `café!()` resolve in crate root and child modules (fresh `Names` and encoded
/// `RecordNames`), while excluded Unicode/ASCII macros fail with `Unresolved::NotFound`.
#[test]
fn macro_use_allowlist_supports_unicode_identifiers() {
    static DEP: &str = "#![no_core]
#[macro_export]
macro_rules! π {
    ($name:ident) => { pub struct $name; };
}
#[macro_export]
macro_rules! café {
    ($name:ident) => { pub struct $name; };
}
#[macro_export]
macro_rules! ω {
    ($name:ident) => { pub struct $name; };
}
";
    static USER_OK: &str = "#![no_core]
macro_rules! if_hyper {
    ($($item:item)*) => { $($item)* };
}
if_hyper! {
    #[macro_use(π, /* comment */ r#café,)]
    extern crate dep;
    π!(FromPi);
    café!(FromCafe);
}
pub mod sub {
    π!(FromSubPi);
    café!(FromSubCafe);
}
";
    static USER_EXCLUDED: &str = "#![no_core]
macro_rules! if_hyper {
    ($($item:item)*) => { $($item)* };
}
if_hyper! {
    #[macro_use(π, café)]
    extern crate dep;
    ω!(Nope);
}
";
    static FILES_OK: Texts = Texts(&[&[("/dep/lib.rs", DEP)], &[("/user/lib.rs", USER_OK)]]);
    static FILES_EXCLUDED: Texts = Texts(&[&[("/dep/lib.rs", DEP)], &[("/user/lib.rs", USER_EXCLUDED)]]);

    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("dep", "/dep/lib.rs", &[]), input("user", "/user/lib.rs", &[("dep", 0)])];

    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), &FILES_OK, &Serial, &Ready::serial(WAIT), bump)
        .unwrap_or_else(|e| panic!("unicode solve: {e:?}"));
    let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
    for (i, c) in all.iter().enumerate() {
        assert!(c.notes.is_empty(), "crate {i}: notes {:?}", c.notes);
        for (m, done) in c.done.iter() {
            assert!(done.scope.errors.is_empty(), "crate {i} module {m:x} errors: {:?}", done.scope.errors);
        }
    }
    let fresh = Names::of_crates(&inputs, Sysroot::default(), &all, g).unwrap_or_else(|e| panic!("names: {e:?}"));
    let records: Vec<&'static [u8]> = all.iter().map(|n| aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump)).collect();
    let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
    let read = RecordNames::new(&keys, Loaded(records), g);
    let user_root = fresh.root(1);
    for segments in [&["FromPi"][..], &["FromCafe"][..], &["sub", "FromSubPi"][..], &["sub", "FromSubCafe"][..]] {
        let w = Written { kind: PathKind::Plain, segments };
        let a = path(&fresh, user_root, Namespace::Types, w, g);
        let b = path(&read, user_root, Namespace::Types, w, g);
        assert!(a.is_ok() && a == b, "unicode {segments:?}: fresh {a:?}, records {b:?}");
    }
    for name in ["π", "café"] {
        for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, user_root, name)) {
            assert!(got.is_ok(), "`{name}` must be in #[macro_use] prelude ({side}): {got:?}");
        }
    }
    for (side, got) in ["fresh", "records"].into_iter().zip(macro_at(&fresh, &read, user_root, "ω")) {
        assert!(not_found(&got), "`ω` must NOT be in #[macro_use] prelude ({side}): {got:?}");
    }

    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), &FILES_EXCLUDED, &Serial, &Ready::serial(WAIT), bump)
        .unwrap_or_else(|e| panic!("excluded unicode solve: {e:?}"));
    let has_unresolved = solved[1].names.done.values().any(|done| {
        done.scope.errors.iter().any(|err| {
            matches!(err, blaze_surface::ScopeError::Macro { why: blaze_surface::Unresolved::NotFound, .. })
        })
    });
    assert!(has_unresolved, "excluded Unicode macro `ω!` must fail with Unresolved::NotFound");
}
