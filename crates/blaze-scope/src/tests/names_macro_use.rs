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
