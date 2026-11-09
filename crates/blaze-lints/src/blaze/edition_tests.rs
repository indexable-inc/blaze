//! The lints of `blaze::edition` (`lossy_as`, `global_state`, `async_fn`): what each finds and
//! leaves alone, and that the group is off until turned on, over an `std` of their own: a
//! `thread_local!` that declares one `const` key per item and calls itself for the rest, as
//! std's does, and an atomic holding an `UnsafeCell`.

use crate::tests::lint;

const MINICORE: &str = "//- minicore: sized, copy, clone, fn, cell, future, option, result, deref, builtin_impls, int_impl\n";

const STD: &str = r#"
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 {
        pub use crate::thread_local;
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod thread {
    pub struct LocalKey<T: 'static> { value: *const T }
    impl<T: 'static> LocalKey<T> {
        pub const fn new() -> LocalKey<T> { loop {} }
        pub fn with<R, F: FnOnce(&T) -> R>(&'static self, f: F) -> R { loop {} }
    }
}
pub mod sync {
    pub mod atomic {
        pub struct AtomicU32 { value: core::cell::UnsafeCell<u32> }
        unsafe impl Sync for AtomicU32 {}
        impl AtomicU32 {
            pub const fn new(value: u32) -> AtomicU32 { AtomicU32 { value: core::cell::UnsafeCell::new(value) } }
            pub fn load(&self) -> u32 { 0 }
        }
    }
}
#[macro_export]
macro_rules! thread_local {
    () => {};
    ($vis:vis static $name:ident: $t:ty = $init:expr; $($rest:tt)*) => {
        $vis const $name: $crate::thread::LocalKey<$t> = $crate::thread::LocalKey::new();
        $crate::thread_local!($($rest)*);
    };
}
"#;

const MAIN: &str = "//- /src/lib.rs crate:main deps:std\n";

/// What `main` is linted to under `flags` over [`STD`].
fn linted(main: &str, flags: &str) -> String {
    let fixture = format!("{MINICORE}{STD}{MAIN}{}", main.trim_start_matches('\n'));
    lint(&fixture, flags)
}

/// What `blaze::<lint_name>` alone says of `main`.
fn only(lint_name: &str, main: &str) -> String {
    linted(main, &format!("-A clippy::all --warn=blaze::{lint_name}"))
}

/// One of each lint's shapes, for the group's tests.
const EACH: &str = r#"
pub fn narrow(x: u64) -> u32 { x as u32 }
pub static mut COUNT: u32 = 0;
pub async fn fetch() -> u32 { 1 }
"#;

/// A cast that can lose is found with both types and what is lost; one that can't, one to its
/// own type, and one of a literal or `const` that fits are not.
#[test]
fn lossy_as_finds_casts_that_can_lose_and_leaves_lossless_ones() {
    let found = only(
        "lossy_as",
        r#"
pub const SMALL: u64 = 200;
pub fn narrow(x: u64) -> u32 { x as u32 }
pub fn sign(x: i32) -> u32 { x as u32 }
pub fn wrap(x: u32) -> i32 { x as i32 }
pub fn fraction(x: f64) -> i32 { x as i32 }
pub fn rounds(x: u64) -> f64 { x as f64 }
pub fn address(p: *const u8) -> usize { p as usize }
pub fn pointer(a: usize) -> *const u8 { a as *const u8 }
pub fn widen(x: u8) -> u32 { x as u32 }
pub fn same(x: u32) -> u32 { x as u32 }
pub fn literal() -> u8 { 255 as u8 }
pub fn constant() -> u8 { SMALL as u8 }
pub fn flag(b: bool) -> u8 { b as u8 }
pub fn float(x: f32) -> f64 { x as f64 }
"#,
    );
    assert_eq!(found.matches("this `as` in").count(), 7, "{found}");
    assert!(found.contains("this `as` in `fn narrow` casts `u64` to `u32`, which drops the high bits"), "{found}");
    assert!(found.contains("`u32::try_from(x)`"), "{found}");
    assert!(found.contains("casts `i32` to `u32`, which turns a negative value into a large positive one"), "{found}");
    assert!(found.contains("`x.cast_unsigned()`"), "{found}");
    assert!(found.contains("casts `u32` to `i32`, which turns a value above `i32::MAX` negative"), "{found}");
    assert!(found.contains("casts `f64` to `i32`, which drops the fraction"), "{found}");
    assert!(found.contains("casts `u64` to `f64`, which rounds"), "{found}");
    assert!(found.contains("casts `*const u8` to `usize`, which keeps the address"), "{found}");
    assert!(found.contains("`ptr.addr()`"), "{found}");
    assert!(found.contains("casts `usize` to `*const u8`, which makes a pointer with no provenance"), "{found}");
    for clean in ["widen", "same", "literal", "constant", "flag", "float"] {
        assert!(!found.contains(&format!("in `fn {clean}`")), "`fn {clean}`: {found}");
    }
}

/// An `async fn` is found once, its `.await`s and the `async` block inside it with it; an
/// `async` block and an `async` closure in a plain function are found each; a plain function
/// is not.
#[test]
fn async_fn_finds_each_outermost_async_once() {
    let found = only(
        "async_fn",
        r#"
pub async fn leaf() -> u32 { 1 }
pub async fn outer() -> u32 {
    let a = leaf().await;
    let b = async { leaf().await }.await;
    a + b
}
pub fn spawns() {
    let _task = async { 1 };
}
pub fn closes() {
    let _f = async || 1;
}
pub fn plain() -> u32 { 1 }
"#,
    );
    assert_eq!(found.matches("which the blaze edition removes").count(), 4, "{found}");
    assert_eq!(found.matches("`async fn outer` is `async`").count(), 1, "{found}");
    assert!(found.contains("`async fn leaf` is `async`"), "{found}");
    assert!(found.contains("an `async` block in `fn spawns` is `async`"), "{found}");
    assert!(found.contains("an `async` closure in `fn closes` is `async`"), "{found}");
    assert!(!found.contains("in `fn outer`"), "{found}");
    assert!(!found.contains("`fn plain`"), "{found}");
}

/// A `static mut` is found with the functions that write and read it, a `static` holding an
/// atomic or a cell with those that use it, and a `thread_local!` item likewise; a `static` of
/// plain data and a `const` are not.
#[test]
fn global_state_finds_mutable_statics_and_thread_locals() {
    let found = only(
        "global_state",
        r#"
use std::sync::atomic::AtomicU32;
pub static mut COUNT: u32 = 0;
pub fn bump() { unsafe { COUNT += 1; } }
pub fn get() -> u32 { unsafe { COUNT } }
pub static HITS: AtomicU32 = AtomicU32::new(0);
pub fn hits() -> u32 { HITS.load() }
pub struct Shared(core::cell::UnsafeCell<u32>);
unsafe impl Sync for Shared {}
pub static CELL: Shared = Shared(core::cell::UnsafeCell::new(0));
thread_local! {
    static DEPTH: u32 = 0;
    static NAME: u8 = 0;
}
pub fn depth() -> u32 { DEPTH.with(|it| *it) }
pub static PLAIN: u32 = 7;
pub const LIMIT: u32 = 8;
pub fn plain() -> u32 { PLAIN + LIMIT }
"#,
    );
    assert_eq!(found.matches("global state").count(), 5, "{found}");
    assert!(
        found.contains("`static mut COUNT: u32` is global state, which the blaze edition removes: written in `fn bump`; read in `fn get`"),
        "{found}"
    );
    assert!(found.contains("`static COUNT: core::sync::atomic::AtomicU32`"), "{found}");
    assert!(found.contains("`static HITS: AtomicU32` has interior mutability"), "{found}");
    assert!(found.contains("used in `fn hits`"), "{found}");
    assert!(found.contains("`static CELL: Shared` has interior mutability"), "{found}");
    assert!(found.contains("nothing in the crate uses it"), "{found}");
    assert!(found.contains("the `thread_local!` `DEPTH: LocalKey<u32>` is global state"), "{found}");
    assert!(found.contains("used in `fn depth`"), "{found}");
    assert!(found.contains("the `thread_local!` `NAME: LocalKey<u8>`"), "{found}");
    assert!(!found.contains("PLAIN"), "{found}");
    assert!(!found.contains("LIMIT"), "{found}");
}

/// The group is allowed by default: none of its lints says anything until it is turned on, and
/// then all three do.
#[test]
fn edition_group_is_off_by_default_and_turns_all_three_on() {
    let off = linted(EACH, "-A clippy::all");
    assert!(!off.contains("blaze::lossy_as"), "{off}");
    assert!(!off.contains("blaze::global_state"), "{off}");
    assert!(!off.contains("blaze::async_fn"), "{off}");
    assert!(!off.contains("which the blaze edition removes"), "{off}");
    assert!(!off.contains("this `as` in"), "{off}");
    let on = linted(EACH, "-A clippy::all --warn=blaze::edition");
    assert!(on.contains("this `as` in `fn narrow` casts `u64` to `u32`"), "{on}");
    assert!(on.contains("`static mut COUNT: u32` is global state"), "{on}");
    assert!(on.contains("`async fn fetch` is `async`"), "{on}");
}
