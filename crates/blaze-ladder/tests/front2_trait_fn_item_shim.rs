//! A NON-survey run that passes a TRAIT'S METHOD as a fn item to a generic `FnOnce` caller:
//! `apply(Clone::clone, &5)`. The fn item's type is `FnDef(Clone::clone, [u8])`; the body that
//! runs is `<u8 as Clone>::clone` (the Reference, "Function item types"; "Associated items": a
//! trait's method on a type runs that type's impl's method). Hello runner 9d's stop, reduced:
//! "`Clone::clone` at [fn item ..<Uint(U8)>, (&Uint(U8),)] reaches item .., which is tabled with
//! no body and is no outside symbol" (the shim was tabled at the trait's bodiless declaration).
//!
//! Three programs, one rule each:
//! - TRAIT_ITEM: `apply(Clone::clone, &5)`: the shim's fn item is a trait's method (the fix).
//! - QUALIFIED: `apply(<u8 as Clone>::clone, &5)`: the same fn item written with its Self.
//! - CONTROL: `apply(own, &5)` with a plain `fn own(x: &u8) -> u8`: a fn item that is no
//!   trait's method resolves to itself and must run as before (fn_item_resolve.rs returns it
//!   unchanged).
//! Each `main` returns 5 as its status. Without crates/blaze-ladder/src/fn_item_resolve.rs's
//! call in run.rs, TRAIT_ITEM and QUALIFIED fail with "tabled with no body"; CONTROL passes
//! either way.
//!
//! Prints: `TRAIT FN ITEM trait_item: ran to status 5`, `.. qualified: ran to status 5`,
//! `.. control: ran to status 5`, then `test trait_fn_items_run_as_the_impls_fn ... ok`.

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

/// The declarations, as core's (clone.rs, marker.rs, ops/function.rs), `no_core`.
const PRELUDE: &str = "#![feature(no_core, lang_items, unboxed_closures, fn_traits, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = \"pointee_sized\"]
pub trait PointeeSized {}
#[lang = \"meta_sized\"]
pub trait MetaSized: PointeeSized {}
#[lang = \"sized\"]
pub trait Sized: MetaSized {}
#[lang = \"legacy_receiver\"]
pub trait LegacyReceiver: PointeeSized {}
impl<T: PointeeSized> LegacyReceiver for &T {}

#[lang = \"clone\"]
pub trait Clone: Sized {
    fn clone(&self) -> Self;
}
#[lang = \"copy\"]
pub trait Copy: Clone {}
impl Clone for u8 {
    fn clone(&self) -> u8 { *self }
}
impl Copy for u8 {}
impl Copy for i32 {}
impl Clone for i32 {
    fn clone(&self) -> i32 { *self }
}

#[lang = \"tuple_trait\"]
pub trait Tuple {}
#[lang = \"fn_once\"]
#[rustc_paren_sugar]
pub trait FnOnce<Args: Tuple> {
    #[lang = \"fn_once_output\"]
    type Output;
    extern \"rust-call\" fn call_once(self, args: Args) -> Self::Output;
}
#[lang = \"fn_mut\"]
#[rustc_paren_sugar]
pub trait FnMut<Args: Tuple>: FnOnce<Args> {
    extern \"rust-call\" fn call_mut(&mut self, args: Args) -> Self::Output;
}
#[lang = \"fn\"]
#[rustc_paren_sugar]
pub trait Fn<Args: Tuple>: FnMut<Args> {
    extern \"rust-call\" fn call(&self, args: Args) -> Self::Output;
}

fn apply<F: FnOnce(&u8) -> u8>(f: F, x: &u8) -> u8 {
    f(x)
}
fn own(x: &u8) -> u8 {
    *x
}
";

fn run(main: &str) -> String {
    let text = format!("{PRELUDE}\n{main}\n");
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    match run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()) {
        Err(e) => format!("{e}"),
        Ok(status) => format!("ran to status {status}"),
    }
}

#[test]
fn trait_fn_items_run_as_the_impls_fn() {
    let cases = [
        ("trait_item", "#[unsafe(no_mangle)] pub extern \"C\" fn main() -> i32 { let five: u8 = 5; apply(Clone::clone, &five) as i32 }"),
        ("qualified", "#[unsafe(no_mangle)] pub extern \"C\" fn main() -> i32 { let five: u8 = 5; apply(<u8 as Clone>::clone, &five) as i32 }"),
        ("control", "#[unsafe(no_mangle)] pub extern \"C\" fn main() -> i32 { let five: u8 = 5; apply(own, &five) as i32 }"),
    ];
    for (name, main) in cases {
        let said = run(main);
        println!("TRAIT FN ITEM {name}: {said}");
        assert!(!said.contains("tabled with no body"), "{name}: the shim was tabled at the trait's declaration: {said}");
        assert_eq!(said, "ran to status 5", "{name}");
    }
}
