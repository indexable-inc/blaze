//! An attribute whose value is a macro call is read from the literal the call expands to.
//!
//! The Reference, "Attributes", "Meta Item Attribute Syntax" (the pinned toolchain's
//! share/doc/rust/html/reference/attributes.html): "Expressions in meta items must macro-expand
//! to literal expressions, which must not include integer or float type suffixes. Expressions
//! which are not literal expressions will be syntactically accepted (and can be passed to
//! proc-macros), but will be rejected after parsing."
//!
//! The real core library (library/core/src/panicking.rs, nightly-2026-10-27, lines 161-179)
//! declares every arithmetic panic so:
//!
//! ```text
//! macro_rules! panic_const {
//!     ($($lang:ident = $message:expr,)+) => {
//!         $(
//!             /// This is a panic called with a message that's a result of a MIR-produced Assert.
//!             //
//!             // never inline unless panic=immediate-abort to avoid code
//!             // bloat at the call sites as much as possible
//!             #[cfg_attr(not(panic = "immediate-abort"), inline(never), cold)]
//!             #[cfg_attr(panic = "immediate-abort", inline)]
//!             #[track_caller]
//!             #[rustc_const_stable_indirect] // must follow stable const rules since it is exposed to stable
//!             #[lang = stringify!($lang)]
//!             pub const fn $lang() -> ! {
//!                 // See the comment in `panic(&'static str)` for why we use `Arguments::from_str` here.
//!                 panic_fmt(fmt::Arguments::from_str($message));
//!             }
//!         )+
//!     }
//! }
//! ```
//!
//! and `panic_const! { panic_const_add_overflow = "attempt to add with overflow", ..,
//! panic_const_div_by_zero = "attempt to divide by zero", .. }`. Before the reader
//! (task/attr_value.rs), a value was only "the first string literal's inside", so these lang
//! items were not filed and every division was refused (`NoLangFn panic_const_div_by_zero`).

use std::vec::Vec;
use std::{println, vec};

use blaze_expand::AttrLiteralError;
use blaze_own::Alloc;

use super::entry::{Texts, input};
use crate::crate_scope::{Ready, resolve_by_records};
use crate::graph::Sysroot;
use crate::resolve::{CrateNames, NamesNote};
use crate::task::{LowerNoteKind, Serial};

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

fn solve_one(text: &'static str) -> &'static CrateNames<'static> {
    let g = super::SyncFrame::leaked();
    let of_crate = vec![("/k0/lib.rs", text)];
    let texts: &'static Texts = std::boxed::Box::leak(std::boxed::Box::new(Texts(Vec::leak(vec![&*Vec::leak(of_crate)]))));
    let inputs = vec![input("k0", "/k0/lib.rs", &[])];
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), texts, &Serial, &Ready::serial(WAIT), g).unwrap_or_else(|e| panic!("solve: {e:?}"));
    solved[0].names
}

/// The crate's lang rows' names, sorted.
fn lang_names(n: &CrateNames<'static>) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = n.tables.langs.iter().map(|l| l.name).collect();
    names.sort_unstable();
    names
}

/// (a) core's own form: a `macro_rules!` that writes `#[lang = stringify!($name)]` for two
/// names files both lang rows under the names. Prints
/// `lang rows from stringify!($name): ["panic_const_add_overflow", "panic_const_div_by_zero"]`.
/// FAILS without the fix: the value holds no quote, so no row is filed (`[]`).
#[test]
fn a_stringify_in_a_macro_rules_expansion_names_the_lang_item() {
    let n = solve_one(
        "#![no_core]\n\
         macro_rules! panic_const { ($($lang:ident)+) => { $( #[lang = stringify!($lang)] pub const fn $lang() -> ! { loop {} } )+ } }\n\
         panic_const! { panic_const_add_overflow panic_const_div_by_zero }\n",
    );
    let names = lang_names(n);
    println!("lang rows from stringify!($name): {names:?}");
    assert_eq!(names, ["panic_const_add_overflow", "panic_const_div_by_zero"]);
}

/// (b) `concat!` of a string and a nested `stringify!` is one literal. Prints
/// `lang row from concat!: ["panic_x"]`.
/// FAILS without the fix: the first quoted text, `panic_`, was taken as the name.
#[test]
fn b_concat_of_a_string_and_a_stringify_names_the_lang_item() {
    let n = solve_one("#![no_core]\n#[lang = concat!(\"panic_\", stringify!(x))]\npub fn f() {}\n");
    let names = lang_names(n);
    println!("lang row from concat!: {names:?}");
    assert_eq!(names, ["panic_x"]);
}

/// (c) `include_str!` needs a file: not evaluated where attributes are read, and refused by
/// name, not read as its argument. Prints
/// `include_str! refused: #[lang = include_str!("x")]: `include_str!` needs a file, ...`.
/// FAILS without the fix: the argument `x` was filed as a lang item, and nothing was refused.
#[test]
fn c_an_include_str_value_is_refused_by_name() {
    let n = solve_one("#![no_core]\n#[lang = include_str!(\"x\")]\npub fn g() {}\n");
    assert_eq!(lang_names(n), Vec::<&str>::new(), "no lang row is filed from include_str!");
    let refusal = n
        .notes
        .iter()
        .find_map(|note| match note {
            NamesNote::Lower(low) => match low.kind {
                LowerNoteKind::AttrValueRefused(r) => Some(r),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_else(|| panic!("no refusal among the notes: {:?}", n.notes));
    println!("include_str! refused: {refusal}");
    assert_eq!(refusal.attr, "lang");
    assert_eq!(refusal.why, AttrLiteralError::NeedsContext { mac: "include_str" });
}

/// (d) the control: a quoted value reads as it always did. Prints `quoted lang row: ["sized"]`.
/// Passes with and without the fix.
#[test]
fn d_a_quoted_lang_value_still_reads() {
    let n = solve_one("#![no_core]\n#[lang = \"sized\"]\npub trait Sized {}\n");
    let names = lang_names(n);
    println!("quoted lang row: {names:?}");
    assert_eq!(names, ["sized"]);
}
