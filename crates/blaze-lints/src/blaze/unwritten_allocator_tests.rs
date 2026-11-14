//! `unwritten_allocator`: a type with an allocator parameter written without it at an item's
//! interface is a finding at every depth (a parameter, a return type, a field, a type alias, an
//! associated type, inside `Result`, `Option`, a tuple, a reference, a slice, a function
//! pointer, `dyn`); `Global` written into a function's signature is the second finding. The
//! function generic over `A: Allocator + Clone`, handed `alloc: A`, passes; so does a method of
//! any trait impl, whose signature is the trait's (found at the trait's declaration when the
//! crate defines it). Each test plants its control: the
//! clean file with the allocator taken back out (or `Global` put in) is found again.
//!
//! And `implicit_allocator`: a value the function returns is its caller's, so the call that
//! makes it gets no `Global` fix, and its note says to take the allocator from the caller.

use super::tests::{ALLOC_STD, MINICORE};
use crate::tests::{lint, offered_in};

/// An `std` whose `Vec`, `Box` and `Arc` take an allocator defaulting to `Global`, as the real
/// one's do, and whose `String` takes none; and a `hir_def` whose trait fixes a signature.
const CRATES: &str = r#"
//- minicore: sized, clone, copy, option, result
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 { pub use core::prelude::v1::*; }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod alloc {
    pub trait Allocator {}
    pub struct Global;
    impl Allocator for Global {}
}
pub mod vec {
    pub struct Vec<T, A: crate::alloc::Allocator = crate::alloc::Global>(*mut T, A);
}
pub mod boxed {
    pub struct Box<T: ?Sized, A: crate::alloc::Allocator = crate::alloc::Global>(*mut T, A);
}
pub mod sync {
    pub struct Arc<T: ?Sized, A: crate::alloc::Allocator = crate::alloc::Global>(*const T, A);
}
pub mod string {
    pub struct String(crate::vec::Vec<u8>);
}
//- /hir_def/lib.rs crate:hir_def deps:std
pub mod deps {
    pub trait PageSource {}
    pub struct ContentId;
    pub trait ImageStore {
        fn pages(&self, key: &ContentId) -> Result<Option<std::sync::Arc<dyn PageSource>>, std::string::String>;
    }
}
//- /src/lib.rs crate:main deps:std,hir_def
"#;

const FLAGS: &str = "-A clippy::all --deny=blaze::unwritten_allocator";

/// The first finding, the allocator left to its default.
const DEFAULTED: &str = "leaves its allocator `A` to its default";

/// The second, `Global` in a function's signature.
const GLOBAL: &str = "chooses the global heap for every caller";

fn found(main: &str, flags: &str) -> String {
    lint(&format!("{}{}", CRATES.trim_start_matches('\n'), main.trim_start_matches('\n')), flags)
}

/// Each type with its allocator left to the default is its own finding, at any depth: an
/// associated type, an alias, a field, inside a tuple, a variant's field, behind a reference,
/// a function pointer's parameter, a slice's element and inside `Option` in a return type. A
/// signature's says to make the function generic and take `alloc: A`; a field's, to name it.
#[test]
fn a_defaulted_allocator_is_found_at_every_depth_of_an_interface() {
    let out = found(
        r#"
use std::boxed::Box;
use std::vec::Vec;
pub trait Source { type Item; }
pub struct Count(u32);
impl Source for Count { type Item = Vec<u8>; }
pub type Bytes = Vec<u8>;
pub struct Index {
    pub names: Vec<u32>,
    pub pair: (u8, Vec<u8>),
}
pub enum Held {
    One(Box<u8>),
}
pub fn take(v: &Vec<u8>, f: fn(Vec<u8>) -> u8, s: &[Vec<u8>]) -> Option<Box<u32>> {
    let _ = (v, f, s);
    loop {}
}
"#,
        FLAGS,
    );
    assert_eq!(out.matches(DEFAULTED).count(), 9, "{out}");
    assert_eq!(out.matches("to its default: make `").count(), 4, "{out}");
    assert_eq!(out.matches("to its default: name it, a parameter `A: Allocator` of its owner").count(), 5, "{out}");
    assert!(out.contains("in the return type of"), "{out}");
    assert!(out.contains("in the parameter `f` of"), "{out}");
    assert!(out.contains("in the type alias"), "{out}");
    assert!(out.contains("bump-allocate"), "{out}");
    assert_eq!(out.matches(GLOBAL).count(), 0, "{out}");
}

/// The owner's signature: `Result<Option<std::sync::Arc<dyn PageSource>>, String>` is two
/// findings, the `Arc` two levels down (`unwritten_allocator`) and the `String` error
/// (`implicit_allocator`, naming the allocator-aware text and a typed error); the generic
/// form, `Arc<dyn PageSource, A>` with `alloc: A` and a typed error, is none.
#[test]
fn the_page_source_signature_is_found_and_its_generic_form_passes() {
    let out = found(
        r#"
use hir_def::deps::{ContentId, PageSource};
use std::alloc::Allocator;
use std::sync::Arc;
pub struct Store;
pub enum PagesError { Missing }
pub fn pages(store: &Store, key: &ContentId) -> Result<Option<std::sync::Arc<dyn hir_def::deps::PageSource>>, std::string::String> {
    let _ = (store, key);
    loop {}
}
pub fn pages_in<A: Allocator + Clone>(store: &Store, key: &ContentId, alloc: A) -> Result<Option<Arc<dyn PageSource, A>>, PagesError> {
    let _ = (store, key, alloc);
    loop {}
}
"#,
        "-A clippy::all --deny=blaze::unwritten_allocator --deny=blaze::implicit_allocator",
    );
    // 1. The `Arc` two levels down, its allocator left to the default.
    assert_eq!(out.matches(DEFAULTED).count(), 1, "{out}");
    assert!(out.contains("`std::sync::Arc<dyn hir_def::deps::PageSource>` in the return type of"), "{out}");
    // 2. The `String` error, with the text type that takes an allocator and a typed error.
    assert_eq!(out.matches("`String` takes no allocator").count(), 1, "{out}");
    assert!(out.contains("`blaze_mem::String<A>`"), "{out}");
    assert!(out.contains("a typed error enum the crate defines"), "{out}");
    // 3. The generic form is none of either.
    assert!(!out.contains("pages_in"), "{out}");
}

/// `Global` in a function's signature is the second finding; a field may name it; a method of
/// any trait impl is left alone, the crate's own trait's or another crate's (its signature is
/// the trait's), and the crate's own trait is found once, at its declaration; the generic form
/// passes. The planted controls write `Global` where the clean file hands `A` in (both places
/// found), and take the field's `Global` out (found).
#[test]
fn global_in_a_signature_is_found_and_a_trait_impl_is_not() {
    const CLEAN: &str = r#"
use std::alloc::{Allocator, Global};
use std::vec::Vec;
use hir_def::deps::{ContentId, ImageStore, PageSource};
pub struct Store;
impl ImageStore for Store {
    fn pages(&self, key: &ContentId) -> Result<Option<std::sync::Arc<dyn PageSource>>, std::string::String> {
        let _ = key;
        loop {}
    }
}
pub trait Local {
    fn made(&self) -> Vec<u8>;
}
impl Local for Store {
    fn made(&self) -> Vec<u8> {
        loop {}
    }
}
pub struct Kept {
    pub bytes: Vec<u8, Global>,
}
pub fn handed<A: Allocator + Clone>(v: Vec<u8, A>, alloc: A) -> Vec<u8, A> {
    let _ = alloc;
    v
}
"#;
    let out = found(CLEAN, FLAGS);
    // The lines of `src/lib.rs` (CLEAN with its first newline trimmed), and what each is:
    // - line 6, `fn pages(..) -> Result<Option<Arc<dyn PageSource>>, String> { .. }`, is a
    //   method of `impl ImageStore for Store` (line 5), a trait of `hir_def`, not a provided
    //   method: its signature is hir_def's, so no finding here (the `String` is
    //   `implicit_allocator`'s, off in FLAGS);
    // - line 12, `fn made(&self) -> Vec<u8>;`, is the declaration in `pub trait Local` (line 11),
    //   the crate's own trait: the one finding, the `Vec` left to its default;
    // - line 15, `fn made(&self) -> Vec<u8> { .. }`, is the method of `impl Local for Store`
    //   (line 14): its signature is line 12's, so no finding;
    // - line 20, the field `Vec<u8, Global>`, names its allocator: no finding (a field may name
    //   `Global`);
    // - line 22, `handed`, generic over `A` and handed `alloc: A`: no finding.
    // Lines 6 and 15 have bodies, so each is linted in its body's query, whose tree is the
    // function alone: what tells they are impl methods is their definition, not a syntax parent.
    assert_eq!(out.matches(DEFAULTED).count(), 1, "the crate's own trait's declaration:\n{out}");
    assert!(out.contains("src/lib.rs:12:"), "at the trait's declaration:\n{out}");
    assert!(!out.contains("src/lib.rs:15:"), "not at the impl of the crate's own trait:\n{out}");
    assert!(!out.contains("src/lib.rs:6:"), "not at the impl of a foreign trait:\n{out}");
    assert_eq!(out.matches(GLOBAL).count(), 0, "{out}");

    let planted = CLEAN.replace("Vec<u8, A>", "Vec<u8, Global>");
    assert_ne!(planted, CLEAN);
    let out = found(&planted, FLAGS);
    assert_eq!(out.matches(GLOBAL).count(), 2, "the control's findings:\n{out}");
    assert!(out.contains("from its caller and write `A` here; the caller names `Global`"), "{out}");

    // The first finding's control: the field's `Global` taken out leaves it to the default.
    let planted = CLEAN.replace("Vec<u8, Global>", "Vec<u8>");
    assert_ne!(planted, CLEAN);
    let out = found(&planted, FLAGS);
    assert_eq!(out.matches(DEFAULTED).count(), 2, "the control's findings:\n{out}");
    assert!(out.contains("in the field"), "{out}");
}

/// The fix writes `Global` out in a field and an alias, and the file it makes is clean; a
/// signature gets none, since the parameter and `alloc: A` change every caller.
#[test]
fn the_fix_writes_global_in_a_field_and_none_in_a_signature() {
    let after = offered_in(
        CRATES.trim_start_matches('\n'),
        "pub struct Kept {\n    pub bytes: std::vec::Vec<u8>,\n}\npub type Bytes = std::boxed::Box<u8>;\n",
        FLAGS,
    );
    assert!(after.contains("pub bytes: std::vec::Vec<u8, std::alloc::Global>"), "{after}");
    assert!(after.contains("pub type Bytes = std::boxed::Box<u8, std::alloc::Global>;"), "{after}");

    let out = found("pub fn made(v: std::vec::Vec<u8>) {\n    let _ = v;\n}\n", FLAGS);
    assert_eq!(out.matches(DEFAULTED).count(), 1, "{out}");
    assert!(!out.contains("write the global heap out"), "{out}");
}

/// `implicit_allocator`: `Vec::new()` whose value the function returns gets no fix to
/// `Vec::new_in(Global)` (which would then be written into the signature), and its note says
/// to take the allocator from the caller; the same call into a local the function drops gets
/// the fix, the control.
#[test]
fn a_returned_value_gets_no_global_fix_and_says_to_take_the_allocator() {
    const MAIN: &str = "//- /src/lib.rs crate:main deps:std\n";
    const FLAGS: &str = "-A clippy::all --deny=blaze::implicit_allocator";
    let returned = "#![feature(allocator_api)]\npub fn made() -> Vec<u32> {\n    Vec::new()\n}\n";
    let out = lint(&format!("{MINICORE}{ALLOC_STD}{MAIN}{returned}"), FLAGS);
    assert!(out.contains("this value is what"), "{out}");
    assert!(out.contains("take `alloc: A` from its caller"), "{out}");
    assert!(!out.contains("use std::alloc::Global;"), "no fix writes `Global`:\n{out}");
    assert!(!out.contains("the better change and is made by hand"), "{out}");

    let dropped = "#![feature(allocator_api)]\npub fn made() {\n    let a: Vec<u32> = Vec::new();\n    let _ = a;\n}\n";
    let out = lint(&format!("{MINICORE}{ALLOC_STD}{MAIN}{dropped}"), FLAGS);
    assert!(out.contains("use std::alloc::Global;"), "the control gets the fix:\n{out}");
    assert!(!out.contains("this value is what"), "{out}");
}
