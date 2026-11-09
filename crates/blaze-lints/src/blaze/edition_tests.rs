//! The lints of `blaze::edition` (`lossy_as`, `global_state`, `async_fn`): what each finds and
//! leaves alone, and that the group is off until turned on, over an `std` of their own: a
//! `thread_local!` that declares one `const` key per item and calls itself for the rest, as
//! std's does, and an atomic holding an `UnsafeCell`. Then the four `unmarked_*` lints, over
//! the effects tests' toolchain (an `Allocator`, a `Vec` that allocates through one, a
//! `Mutex::lock`, `env::var` and `SystemTime::now` at the paths the models name): what each
//! finds, what it leaves, and the one fix of a function.

use std::path::PathBuf;

use base_db::FileId;
use hir_expand::fixture;

use super::effects_tests::{MINICORE as EFFECTS_MINICORE, TOOLCHAIN};
use crate::{
    check::{Setup, check},
    conf::Conf,
    fix::fix,
    level::CommandLine,
    tests::{lint, lint_with, main_crate},
};

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

// unmarked_pure, unmarked_nopanic, unmarked_noalloc, unmarked_nonblocking

const UNMARKED: [&str; 4] =
    ["unmarked_pure", "unmarked_nopanic", "unmarked_noalloc", "unmarked_nonblocking"];

/// `main` as `src/lib.rs` of the crate `main` over the effects tests' toolchain, its crate's
/// meta `meta` after `crate:main` (` cfg:overflow_checks` for a crate with overflow checks).
fn effects_fixture(meta: &str, main: &str) -> String {
    format!(
        "{EFFECTS_MINICORE}{TOOLCHAIN}//- /src/lib.rs crate:main deps:alloc,std{meta}\n{}",
        main.trim_start_matches('\n')
    )
}

/// The flags that warn of `lints` alone.
fn warn_of(lints: &[&str]) -> String {
    lints.iter().fold("-A clippy::all".to_owned(), |flags, it| format!("{flags} --warn=blaze::{it}"))
}

/// What `lints` say of `main` under `conf`.
fn unmarked(lints: &[&str], meta: &str, conf: Conf, main: &str) -> String {
    lint_with(&effects_fixture(meta, main), &warn_of(lints), conf)
}

/// `clippy.toml` with `unmarked-effects-private = true`.
fn private_too() -> Conf {
    let text = "unmarked-effects-private = true\n".to_owned();
    Conf::of_text(Some((PathBuf::from("clippy.toml"), text)), None).expect("the key is a flag")
}

/// How many findings `out` has of `fn name`.
fn proved(out: &str, name: &str) -> usize {
    let marker = format!("`fn {name}` is proved ");
    out.lines().filter(|line| line.contains(&marker)).count()
}

/// `main` after `blaze check --fix` with `lints` on, which then have nothing left to say.
fn fixed(lints: &[&str], meta: &str, main: &str) -> String {
    let fx = fixture::db(&effects_fixture(meta, main));
    let file = *fx.files().last().expect("src/lib.rs");
    let mut db = fx.into_db();
    let krate = blaze_query::db::attach(&db, || main_crate(&db));
    let file = file.file_id(&db);
    let name_of = |it: FileId| (it == file).then(|| "src/lib.rs".to_owned());
    let flags: Vec<String> = warn_of(lints).split_whitespace().map(str::to_owned).collect();
    let setup = Setup {
        command_line: CommandLine::parse(&flags).expect("the flags read"),
        conf: Conf::default(),
    };
    fix(&mut db, krate, &setup, &name_of).unwrap_or_else(|error| panic!("{error}"));
    let text = db.file_text(file).text(&db).to_string();
    blaze_query::db::attach(&db, || {
        let found = check(&db, krate, &setup, &name_of).expect("the file is named");
        let left: Vec<&str> = found
            .iter()
            .map(|it| it.diagnostic.message.as_str())
            .filter(|it| it.contains(" is proved "))
            .collect();
        assert!(left.is_empty(), "the fixed file has nothing left to mark:\n{text}\n{left:#?}");
    });
    text
}

/// A pure `pub fn` is found once, at its name, saying what the mark gives; the fix writes
/// `#[blaze::pure]` on its own line above the signature, under the doc comment and the
/// attribute already there.
#[test]
fn unmarked_pure_finds_a_pure_pub_fn_and_its_fix_marks_it() {
    const MAIN: &str = r#"
/// The two mixed.
#[inline]
pub fn mix(a: u32, b: u32) -> u32 {
    a ^ b
}
"#;
    let out = unmarked(&["unmarked_pure"], "", Conf::default(), MAIN);
    assert_eq!(proved(&out, "mix"), 1, "{out}");
    assert!(
        out.contains(
            "`fn mix` is proved pure through every call it makes, and is not marked `#[blaze::pure]`: marking it lets callers and the cache rely on it"
        ),
        "{out}"
    );
    assert!(out.contains("blaze::unmarked_pure"), "{out}");
    assert!(!out.contains("also stands for"), "one lint is on:\n{out}");
    let text = fixed(&["unmarked_pure"], "", MAIN);
    let want = r#"
/// The two mixed.
#[inline]
#[blaze::pure]
pub fn mix(a: u32, b: u32) -> u32 {
    a ^ b
}
"#;
    assert_eq!(text, want.trim_start_matches('\n'));
}

/// A function that reads the environment or the clock, here or through a call, is not pure
/// and is not found.
#[test]
fn unmarked_pure_leaves_what_the_pure_check_refuses() {
    let out = unmarked(
        &["unmarked_pure"],
        "",
        Conf::default(),
        r#"
pub fn mode() -> bool {
    match std::env::var("MODE") {
        Ok(_) => true,
        Err(_) => false,
    }
}
fn stamp() -> u32 {
    let _ = std::time::SystemTime::now();
    1
}
pub fn stamped() -> u32 {
    stamp()
}
"#,
    );
    assert!(!out.contains(" is proved "), "{out}");
}

/// A function already marked is not found: the bare attribute, `trusted`, and a method of an
/// impl of a marked trait. A method of an impl of an unmarked `pub` trait is found, as the
/// check reads the function's own attribute first.
#[test]
fn unmarked_pure_leaves_the_marked_and_finds_an_impl_method_of_an_unmarked_trait() {
    let out = unmarked(
        &["unmarked_pure"],
        "",
        Conf::default(),
        r#"
#[blaze::pure]
pub fn checked(a: u32) -> u32 {
    a
}
#[blaze::pure(trusted = "read by hand")]
pub fn believed(a: u32) -> u32 {
    a
}
#[blaze::pure]
pub trait Step {
    fn run(input: u32) -> u32;
}
pub struct Same;
impl Step for Same {
    fn run(input: u32) -> u32 {
        input
    }
}
pub trait Plain {
    fn go(input: u32) -> u32;
}
impl Plain for Same {
    fn go(input: u32) -> u32 {
        input
    }
}
"#,
    );
    assert_eq!(proved(&out, "checked"), 0, "{out}");
    assert_eq!(proved(&out, "believed"), 0, "{out}");
    assert_eq!(proved(&out, "run"), 0, "its trait is marked:\n{out}");
    assert_eq!(proved(&out, "go"), 1, "{out}");
}

/// A function with no visibility written is found only with `unmarked-effects-private = true`.
#[test]
fn unmarked_pure_finds_a_private_fn_only_with_the_key() {
    const MAIN: &str = r#"
fn inner(a: u32) -> u32 {
    a
}
pub(crate) fn shared(a: u32) -> u32 {
    inner(a)
}
"#;
    let border = unmarked(&["unmarked_pure"], "", Conf::default(), MAIN);
    assert_eq!(proved(&border, "inner"), 0, "{border}");
    assert_eq!(proved(&border, "shared"), 1, "a visibility is written:\n{border}");
    let every = unmarked(&["unmarked_pure"], "", private_too(), MAIN);
    assert_eq!(proved(&every, "inner"), 1, "{every}");
    assert_eq!(proved(&every, "shared"), 1, "{every}");
}

/// A callee that is pure by inference and not marked does not stop its caller: the check
/// walks the callee's body whether or not it is marked, so the caller is found.
#[test]
fn unmarked_pure_finds_a_caller_of_an_unmarked_pure_callee() {
    let out = unmarked(
        &["unmarked_pure"],
        "",
        Conf::default(),
        r#"
fn helper(a: u32) -> u32 {
    a ^ 1
}
pub fn outer(a: u32) -> u32 {
    helper(a)
}
"#,
    );
    assert_eq!(proved(&out, "outer"), 1, "{out}");
    assert_eq!(proved(&out, "helper"), 0, "private:\n{out}");
}

/// A generic function is proved as a marked one is checked, in its own where-clauses: what
/// only an instance decides is left to each instance, and the finding says so. One that reads
/// the environment is refused whatever the instance.
#[test]
fn unmarked_pure_finds_a_generic_fn_leaving_its_instances_open() {
    let out = unmarked(
        &["unmarked_pure"],
        "",
        Conf::default(),
        r#"
pub fn same<T>(a: T) -> T {
    a
}
pub fn reads<T>(a: T) -> T {
    let _ = std::env::var("MODE");
    a
}
"#,
    );
    assert_eq!(proved(&out, "same"), 1, "{out}");
    assert!(out.contains("is left to each instance"), "{out}");
    assert_eq!(proved(&out, "reads"), 0, "{out}");
}

/// A test function and the crate's `main` are not found.
#[test]
fn unmarked_pure_leaves_tests_and_main() {
    let out = unmarked(
        &["unmarked_pure"],
        "",
        private_too(),
        r#"
#[test]
pub fn checks() {}
pub fn main() {}
"#,
    );
    assert!(!out.contains(" is proved "), "{out}");
}

/// `unmarked_nopanic` finds only in a crate checked with overflow checks: there an index a
/// constant decides is no panic, and a kept bounds check and `x + 1` are. Checked without
/// them nothing is found, as `x + 1` would pass and then be refused in dev.
#[test]
fn unmarked_nopanic_finds_only_with_overflow_checks() {
    const MAIN: &str = r#"
pub fn third(a: &[u32; 4]) -> u32 {
    a[2]
}
pub fn at(v: &[u32], i: usize) -> u32 {
    v[i]
}
pub fn bump(x: u32) -> u32 {
    x + 1
}
"#;
    let checked = unmarked(&["unmarked_nopanic"], " cfg:overflow_checks", Conf::default(), MAIN);
    assert_eq!(proved(&checked, "third"), 1, "{checked}");
    assert!(
        checked.contains("`fn third` is proved nopanic through every call it makes, and is not marked `#[blaze::nopanic]`: marking it lets callers rely on it"),
        "{checked}"
    );
    assert_eq!(proved(&checked, "at"), 0, "{checked}");
    assert_eq!(proved(&checked, "bump"), 0, "{checked}");
    let unchecked = unmarked(&["unmarked_nopanic"], "", Conf::default(), MAIN);
    assert!(!unchecked.contains(" is proved "), "{unchecked}");
}

/// `unmarked_noalloc` finds a function that never reaches the global heap and leaves one that
/// does.
#[test]
fn unmarked_noalloc_finds_what_never_reaches_the_global_heap() {
    let out = unmarked(
        &["unmarked_noalloc"],
        "",
        Conf::default(),
        r#"
pub fn count(x: u32) -> u32 {
    x
}
pub fn heap() -> alloc::vec::Vec<u32> {
    alloc::vec::Vec::with_capacity(4)
}
"#,
    );
    assert_eq!(proved(&out, "count"), 1, "{out}");
    assert!(out.contains("`fn count` is proved noalloc"), "{out}");
    assert_eq!(proved(&out, "heap"), 0, "{out}");
}

/// In a `#![blaze::capabilities]` crate the global heap is out of reach, so a signature with
/// no way to allocate already says noalloc and is not found; one handed a `Vec` over an
/// allocator, which it does not allocate through, is. Without the attribute both are found.
#[test]
fn unmarked_noalloc_in_a_capabilities_crate_finds_only_a_signature_that_could_allocate() {
    const MAIN: &str = r#"
pub fn f(x: u32) -> u32 {
    x
}
pub fn g<A: core::alloc::Allocator>(v: &mut alloc::vec::Vec<u8, A>) -> usize {
    v.len()
}
"#;
    let capable = format!("#![blaze::capabilities]\n{}", MAIN.trim_start_matches('\n'));
    let out = unmarked(&["unmarked_noalloc"], "", Conf::default(), &capable);
    assert_eq!(proved(&out, "f"), 0, "its signature can't allocate:\n{out}");
    assert_eq!(proved(&out, "g"), 1, "{out}");
    let plain = unmarked(&["unmarked_noalloc"], "", Conf::default(), MAIN);
    assert_eq!(proved(&plain, "f"), 1, "{plain}");
    assert_eq!(proved(&plain, "g"), 1, "{plain}");
}

/// `unmarked_nonblocking` finds a function that never waits and leaves one that takes a lock.
#[test]
fn unmarked_nonblocking_finds_what_never_blocks() {
    let out = unmarked(
        &["unmarked_nonblocking"],
        "",
        Conf::default(),
        r#"
pub fn quick(x: u32) -> u32 {
    x
}
pub fn waits(m: &std::sync::Mutex<u32>) -> u32 {
    let _guard = m.lock();
    1
}
"#,
    );
    assert_eq!(proved(&out, "quick"), 1, "{out}");
    assert!(out.contains("`fn quick` is proved nonblocking"), "{out}");
    assert_eq!(proved(&out, "waits"), 0, "{out}");
}

/// A function all four hold of has one finding, under `unmarked_pure`, naming all four, and
/// one fix that writes the four attributes, one per line at the item's indentation, in the
/// order pure, nopanic, noalloc, nonblocking.
#[test]
fn a_function_all_four_hold_of_has_one_finding_and_one_fix() {
    const MAIN: &str = r#"
pub struct Pair(u32, u32);
impl Pair {
    /// The two mixed.
    pub fn mix(&self) -> u32 {
        self.0 ^ self.1
    }
}
"#;
    let out = unmarked(&UNMARKED, " cfg:overflow_checks", Conf::default(), MAIN);
    assert_eq!(proved(&out, "mix"), 1, "{out}");
    assert!(
        out.contains(
            "`fn mix` is proved pure, nopanic, noalloc and nonblocking through every call it makes, and is not marked `#[blaze::pure]`, `#[blaze::nopanic]`, `#[blaze::noalloc]` and `#[blaze::nonblocking]`"
        ),
        "{out}"
    );
    assert!(
        out.contains("also stands for `blaze::unmarked_nopanic`, `blaze::unmarked_noalloc` and `blaze::unmarked_nonblocking`"),
        "{out}"
    );
    let text = fixed(&UNMARKED, " cfg:overflow_checks", MAIN);
    let want = r#"
pub struct Pair(u32, u32);
impl Pair {
    /// The two mixed.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn mix(&self) -> u32 {
        self.0 ^ self.1
    }
}
"#;
    assert_eq!(text, want.trim_start_matches('\n'));
}

/// The four are in `blaze::edition`: silent until the group, or one of them, is turned on.
#[test]
fn the_unmarked_lints_are_off_until_the_edition_group_is_on() {
    const MAIN: &str = r#"
pub fn mix(a: u32, b: u32) -> u32 {
    a ^ b
}
"#;
    let off = lint(&effects_fixture("", MAIN), "-A clippy::all");
    assert!(!off.contains(" is proved "), "{off}");
    let on = lint(&effects_fixture("", MAIN), "-A clippy::all --warn=blaze::edition");
    assert_eq!(proved(&on, "mix"), 1, "{on}");
    assert!(on.contains("`fn mix` is proved pure, noalloc and nonblocking"), "no overflow checks, so no nopanic:\n{on}");
}
