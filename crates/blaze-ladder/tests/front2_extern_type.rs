//! An extern type (`extern { type T; }`, feature `extern_types`): core's
//! `unsafe extern "C" { #[lang = "code"] pub type Code; }` (ops/function.rs), the pointee of
//! `FnPtr::as_ptr(self) -> NonNull<Code>`. Every program that reads core met it, and heads.rs
//! `alias` refused it as a type alias with no `= ..` (25 tests of the 2026-11-21 round).
//!
//! The rules (the Reference, "External blocks" / the `extern_types` RFC 1861): an extern type
//! is a type of its own, equal only to itself; it has no size or alignment known to the
//! compiler (not `Sized`, not `MetaSized`: only `PointeeSized`), so it is only ever behind a
//! pointer, and a pointer to it is THIN; it takes no generic parameters (E0044); it implements
//! no auto trait unless an impl says so.
//!
//! The fix is `PNode::Foreign(ItemKey)`, the checker's node for the solver's `TyKind::Foreign`.
//! The programs are `#![no_core]`, built and run in this process.

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };

/// The sizedness lang items (core's marker.rs) and `Copy` of `u8`, so `*back` reads a byte.
const HEAD: &str = "#![feature(no_core, lang_items, extern_types)]
#![no_core]
#![no_main]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"clone\"]
trait Clone: Sized {
    fn clone(&self) -> Self;
}

#[lang = \"copy\"]
trait Copy: Clone {}

impl Clone for u8 {
    fn clone(&self) -> u8 {
        *self
    }
}
impl Copy for u8 {}

/// A generic fn at the extern type: its instance key and symbol hold the type's node.
fn id<T: PointeeSized>(p: *const T) -> *const T {
    p
}
";

/// The real site's shape: a field and an instance argument whose type is the extern type.
const EXTERN_CODE: &str = "
unsafe extern \"C\" {
    pub type Code;
}
";

/// Builds and runs `HEAD` + `decl` + `main`: the status, or the refusal on one line.
fn run(decl: &str, main: &str) -> Result<u8, String> {
    let text = [HEAD, decl, main].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| {
        let said = format!("{e}").replace('\n', " ");
        said.char_indices().nth(400).map_or(said.clone(), |(i, _)| said[..i].to_string())
    })
}

/// A byte's address cast to `*const Code` (thin to thin), held in a struct's field, passed
/// through `id::<Code>`, cast back and read: 7.
const THROUGH: &str = "
struct Held {
    at: *const Code,
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let x: u8 = 7;
    let p = &x as *const u8 as *const Code;
    let h = Held { at: id::<Code>(p) };
    let back = h.at as *const u8;
    unsafe { *back as i32 }
}
";

/// `need`'s `T` has the implicit `T: Sized` bound.
const NEEDS_SIZED: &str = "
fn need<T>(_p: *const T) {}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let x: u8 = 7;
    let p = &x as *const u8 as *const Code;
    need::<Code>(p);
    7
}
";

/// The fix: the extern type is a type, a pointer to it thin; the program builds and runs.
/// Prints `EXTERN TYPE: Ok(7)`.
#[test]
fn a_pointer_to_an_extern_type_is_held_passed_and_cast_back() {
    let got = run(EXTERN_CODE, THROUGH);
    println!("EXTERN TYPE: {got:?}");
    assert_eq!(got, Ok(7));
}

/// The twin rustc rejects: an extern type is not `Sized`, so `need::<Code>` is E0277 at the
/// check, never a build.
/// Prints `NOT SIZED: Err(..E0277..)`.
#[test]
fn an_extern_type_is_not_sized() {
    let got = run(EXTERN_CODE, NEEDS_SIZED);
    println!("NOT SIZED: {got:?}");
    let said = got.expect_err("an extern type is not `Sized`");
    assert!(said.contains("E0277") || said.contains("Unsatisfied"), "{said}");
}

/// The twin rustc rejects: an extern type takes no generic parameters (E0044), refused by name.
/// Prints `GENERIC EXTERN TYPE: Err(..E0044..)`.
#[test]
fn a_generic_extern_type_is_e0044() {
    let decl = "
unsafe extern \"C\" {
    pub type Code<T>;
}
";
    let main = "
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let x: u8 = 7;
    let p = &x as *const u8 as *const Code<u8>;
    let back = p as *const u8;
    unsafe { *back as i32 }
}
";
    let got = run(decl, main);
    println!("GENERIC EXTERN TYPE: {got:?}");
    let said = got.expect_err("an extern type takes no generic parameters");
    assert!(said.contains("E0044"), "{said}");
}

/// The control (rule 23): the same two programs with `Code` an ordinary (sized) struct. The
/// first gives the same 7, so the answer is the pointer's flow and not the node; the second now
/// builds, so the twin's E0277 is the extern type's, removed with it.
/// Prints `SIZED STRUCT: Ok(7) Ok(7)`.
#[test]
fn control_an_ordinary_struct_in_the_same_places() {
    let decl = "
struct Code;
";
    let through = run(decl, THROUGH);
    let sized = run(decl, NEEDS_SIZED);
    println!("SIZED STRUCT: {through:?} {sized:?}");
    assert_eq!(through, Ok(7));
    assert_eq!(sized, Ok(7));
}
