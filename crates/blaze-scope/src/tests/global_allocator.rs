//! `#[global_allocator]` on a static is a call of the prelude's built-in attribute macro (the
//! pinned library, core/src/macros/mod.rs: `#[rustc_builtin_macro] pub macro
//! global_allocator($item:item)`). Its expansion (blaze-expand global_allocator.rs) is the static
//! as written and, beside it in the same module, the four allocator fns alloc's `unsafe extern
//! "Rust"` block declares (library/alloc/src/alloc.rs: `__rust_alloc`, `__rust_dealloc`,
//! `__rust_realloc`, `__rust_alloc_zeroed`). The static stays filed in the lang rows under
//! `global_allocator`, keyed as the expansion declares it: blaze-ladder front2/alloc_shim.rs asks
//! that row and binds each declaration to the fn of its name in the static's crate.

use std::vec::Vec;
use std::{format, println};

use super::entry::{Texts, input};
use crate::crate_scope::{Ready, resolve_by_records};
use crate::graph::Sysroot;
use crate::task::Serial;

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// A crate that defines the macro as core does (so its `$crate` is this crate) and declares its
/// own allocator.
const USER: &str = "#![no_core]
#![feature(no_core, rustc_attrs, decl_macro)]
#[rustc_builtin_macro]
pub macro global_allocator($item:item) {}
pub struct Counting;
#[global_allocator]
static ALLOC: Counting = Counting;
";

static TEXTS: Texts = Texts(&[&[("/user/lib.rs", USER)]]);

/// Rule: the static and the four `__rust_*` fns are items of the root module, each once, and the
/// `global_allocator` lang row names the static the expansion declares (its key, not the
/// call's input's).
///
/// Prints `global allocator expansion in the root: ["ALLOC", "__rust_alloc", "__rust_alloc_zeroed",
/// "__rust_dealloc", "__rust_realloc"]; lang row names ALLOC` and
/// `test tests::global_allocator::a_global_allocator_declares_the_static_and_the_four_fns ... ok`.
/// FAILS with the attribute read as inert (the fns are never written: only `ALLOC`), and with the
/// lang row keyed by the input item (that key names no item text).
#[test]
fn a_global_allocator_declares_the_static_and_the_four_fns() {
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let crates = [input("user", "/user/lib.rs", &[])];
    let (all, _) = resolve_by_records(&crates, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), erased.bump())
        .unwrap_or_else(|e| panic!("by records: {e:?}"));
    let user = all[0].names;
    let wanted = ["ALLOC", "__rust_alloc", "__rust_alloc_zeroed", "__rust_dealloc", "__rust_realloc"];
    let mut in_root: Vec<&str> = user
        .tables
        .texts
        .values()
        .filter(|(t, _)| t.module == user.root)
        .filter_map(|(t, _)| t.name)
        .filter(|name| wanted.contains(name))
        .collect();
    in_root.sort_unstable();
    assert_eq!(in_root, wanted, "the static and the four fns, each once, in the root; notes {:?}", user.notes);
    let rows: Vec<_> = user.tables.langs.iter().filter(|l| l.name == "global_allocator").collect();
    assert_eq!(rows.len(), 1, "one `global_allocator` lang row: {rows:?}");
    let named = user.tables.texts.get(&rows[0].key).and_then(|(t, _)| t.name);
    assert_eq!(named, Some("ALLOC"), "{}", format!("the row names the expansion's static: {:?}", rows[0]));
    println!("global allocator expansion in the root: {in_root:?}; lang row names ALLOC");
}
