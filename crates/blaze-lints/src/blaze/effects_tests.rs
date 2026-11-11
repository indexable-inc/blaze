//! `nopanic`, `noalloc` and `nonblocking` over a toolchain of their own: minicore's `core` with
//! its `Allocator`, an `alloc` whose `Global` ends in `__rust_alloc` and whose `Vec` allocates
//! through any `Allocator`, and a `std` whose `Mutex::lock` is at the path the models name.
//! Per effect, a crate that passes and one test per kind of refusal; then `trusted`, a marked
//! trait and a malformed attribute. Each refusal is said once, at the marked function, naming
//! the line of what refuses it.

use crate::tests::lint;

pub(super) const MINICORE: &str = "//- minicore: sized, copy, clone, drop, option, result, fn, panic, index, slice, builtin_impls, allocator_api\n";

pub(super) const TOOLCHAIN: &str = r#"
//- /alloc.rs crate:alloc
pub mod alloc {
    unsafe extern "Rust" {
        #[rustc_allocator]
        #[rustc_nounwind]
        #[rustc_std_internal_symbol]
        fn __rust_alloc(size: usize, align: usize) -> *mut u8;
    }
    pub struct Global;
    unsafe impl core::alloc::Allocator for Global {
        fn allocate(&self, _layout: core::alloc::Layout) -> Result<*mut u8, core::alloc::AllocError> {
            Ok(unsafe { __rust_alloc(8, 8) })
        }
        unsafe fn deallocate(&self, _ptr: *mut u8, _layout: core::alloc::Layout) {}
    }
}
pub mod vec {
    pub struct Vec<T, A: core::alloc::Allocator = crate::alloc::Global> {
        ptr: *mut T,
        len: usize,
        alloc: A,
    }
    impl<T> Vec<T> {
        pub fn with_capacity(n: usize) -> Vec<T> {
            Vec::with_capacity_in(n, crate::alloc::Global)
        }
    }
    impl<T, A: core::alloc::Allocator> Vec<T, A> {
        pub fn with_capacity_in(n: usize, alloc: A) -> Vec<T, A> {
            let layout = unsafe { core::alloc::Layout::from_size_align_unchecked(n, 8) };
            match alloc.allocate(layout) {
                Ok(ptr) => Vec { ptr: ptr as *mut T, len: 0, alloc },
                Err(_) => loop {},
            }
        }
    }
}
//- /std.rs crate:std deps:alloc
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod sync {
    pub struct Mutex<T> {
        value: T,
    }
    pub struct MutexGuard<'a, T>(&'a Mutex<T>);
    impl<T> Mutex<T> {
        pub fn lock(&self) -> MutexGuard<'_, T> {
            MutexGuard(self)
        }
        pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
            None
        }
    }
}
pub mod env {
    pub struct VarError;
    pub fn var(_key: &str) -> Result<u32, VarError> {
        loop {}
    }
}
pub mod time {
    pub struct SystemTime(u64);
    impl SystemTime {
        pub fn now() -> SystemTime {
            loop {}
        }
    }
}
"#;

/// What the three lints say of `main` (`src/lib.rs`, its crate's meta `meta` after `crate:main`).
fn effects_of(meta: &str, main: &str) -> String {
    let fixture = format!(
        "{MINICORE}{TOOLCHAIN}//- /src/lib.rs crate:main deps:alloc,std{meta}\n{}",
        main.trim_start_matches('\n')
    );
    lint(
        &fixture,
        "-A clippy::all --deny=blaze::nopanic --deny=blaze::noalloc --deny=blaze::nonblocking",
    )
}

/// The findings of `lint_name` in `out`.
fn findings<'o>(out: &'o str, lint_name: &str) -> Vec<&'o str> {
    let marker = format!("is `#[blaze::{lint_name}]`");
    out.lines().filter(|line| line.contains(&marker)).collect()
}

// nopanic

/// Indexing an array by a constant inside it, `^`, and a call of a function that is itself
/// clean are no panic: the bounds check is decided by the constants and folded.
#[test]
fn nopanic_passes_what_cannot_panic() {
    let out = effects_of(
        "",
        r#"
fn mix(a: u32, b: u32) -> u32 {
    a ^ b
}
#[blaze::nopanic]
pub fn third(a: &[u32; 4]) -> u32 {
    mix(a[2], a[0])
}
"#,
    );
    assert_eq!(findings(&out, "nopanic"), Vec::<&str>::new(), "{out}");
}

/// An index the block makes a constant is folded only while nothing else can write it: one
/// rewritten through a pointer before the check is a kept bounds check.
#[test]
fn nopanic_refuses_an_index_written_through_a_pointer() {
    let out = effects_of(
        "",
        r#"
#[blaze::nopanic]
pub fn moved(a: &[u32; 4]) -> u32 {
    let mut i = 2;
    let p = &raw mut i;
    unsafe { *p = 9 };
    a[i]
}
"#,
    );
    let found = findings(&out, "nopanic");
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("checks an index against a length"), "{out}");
}

/// A bounds check the MIR keeps, two calls down, is refused at the marked function with the
/// chain to it and the index's line; the same index in an unmarked function is not checked.
#[test]
fn nopanic_refuses_a_kept_bounds_check_naming_its_line() {
    let out = effects_of(
        "",
        r#"
fn pick(v: &[u32], i: usize) -> u32 {
    v[i]
}
#[blaze::nopanic]
pub fn first(v: &[u32]) -> u32 {
    pick(v, 0)
}
pub fn unmarked(v: &[u32], i: usize) -> u32 {
    v[i]
}
"#,
    );
    let found = findings(&out, "nopanic");
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("`main::first` → `main::pick`"), "the chain:\n{out}");
    assert!(out.contains("lib.rs:2"), "the index's line:\n{out}");
}

/// `x + 1` is refused where the crate keeps overflow checks and passes where it doesn't, and
/// the finding says which profile it checked.
#[test]
fn nopanic_holds_overflow_to_the_crates_profile() {
    const BUMP: &str = r#"
#[blaze::nopanic]
pub fn bump(x: u32) -> u32 {
    x + 1
}
"#;
    let checked = effects_of(" cfg:overflow_checks", BUMP);
    let found = findings(&checked, "nopanic");
    assert_eq!(found.len(), 1, "{checked}");
    assert!(found[0].contains("with overflow checks"), "{checked}");
    let unchecked = effects_of("", BUMP);
    assert_eq!(findings(&unchecked, "nopanic"), Vec::<&str>::new(), "{unchecked}");
}

/// A foreign function whose ABI may unwind is refused; one whose ABI can't (`"C"`) is not a
/// panic, though nothing else is known of it.
#[test]
fn nopanic_refuses_a_foreign_call_that_may_unwind() {
    let out = effects_of(
        "",
        r#"
unsafe extern "C-unwind" {
    fn callback();
}
unsafe extern "C" {
    fn quiet();
}
#[blaze::nopanic]
pub fn unwinds() {
    unsafe { callback() }
}
#[blaze::nopanic]
pub fn stays() {
    unsafe { quiet() }
}
"#,
    );
    let found = findings(&out, "nopanic");
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("`main::unwinds`") && found[0].contains("may unwind"), "{out}");
}

// noalloc

/// A `Vec` made in a caller's frame (an `Allocator` the code names) is no global allocation.
#[test]
fn noalloc_passes_an_explicit_allocator() {
    let out = effects_of(
        "",
        r#"
pub struct Frame {
    base: *mut u8,
}
unsafe impl core::alloc::Allocator for &Frame {
    fn allocate(&self, _layout: core::alloc::Layout) -> Result<*mut u8, core::alloc::AllocError> {
        Ok(self.base)
    }
    unsafe fn deallocate(&self, _ptr: *mut u8, _layout: core::alloc::Layout) {}
}
#[blaze::noalloc]
pub fn scratch(frame: &Frame) -> alloc::vec::Vec<u32, &Frame> {
    alloc::vec::Vec::with_capacity_in(4, frame)
}
"#,
    );
    assert_eq!(findings(&out, "noalloc"), Vec::<&str>::new(), "{out}");
}

/// A raw pointer hides only memory, which `noalloc` doesn't read: a `dyn` behind one still
/// hides code, refused.
#[test]
fn noalloc_refuses_a_dyn_behind_a_raw_pointer() {
    let out = effects_of(
        "",
        r#"
pub trait Job {
    fn run(&self);
}
#[blaze::noalloc]
pub fn call(job: *const dyn Job) {
    unsafe { (*job).run() }
}
"#,
    );
    let found = findings(&out, "noalloc");
    assert_eq!(found.len(), 1, "{out}");
    assert!(
        found[0].contains("takes `dyn Job") && found[0].contains("whose calls the check can't see"),
        "{out}"
    );
    // The type as a person writes it, not the solver's debug form (`dyn [Binder { .. }]`).
    assert!(!found[0].contains("Binder"), "{out}");
}

/// The same `Vec` through `Global` is refused: the global allocator, through `Vec`'s own code.
#[test]
fn noalloc_refuses_the_global_allocator() {
    let out = effects_of(
        "",
        r#"
#[blaze::noalloc]
pub fn heap() -> alloc::vec::Vec<u32> {
    alloc::vec::Vec::with_capacity(4)
}
"#,
    );
    let found = findings(&out, "noalloc");
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("global allocator"), "{out}");
}

/// A foreign function the table does not know may allocate: refused, by its name.
#[test]
fn noalloc_refuses_an_unknown_foreign_function() {
    let out = effects_of(
        "",
        r#"
unsafe extern "C" {
    fn mystery() -> u32;
}
#[blaze::noalloc]
pub fn ask() -> u32 {
    unsafe { mystery() }
}
"#,
    );
    let found = findings(&out, "noalloc");
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("mystery"), "{out}");
}

// nonblocking

/// `try_lock` never waits.
#[test]
fn nonblocking_passes_a_try_lock() {
    let out = effects_of(
        "",
        r#"
#[blaze::nonblocking]
pub fn peek(m: &std::sync::Mutex<u32>) -> bool {
    m.try_lock().is_some()
}
"#,
    );
    assert_eq!(findings(&out, "nonblocking"), Vec::<&str>::new(), "{out}");
}

/// `Mutex::lock` is in the blocking set: refused, naming the call and why it blocks.
#[test]
fn nonblocking_refuses_a_lock() {
    let out = effects_of(
        "",
        r#"
#[blaze::nonblocking]
pub fn take(m: &std::sync::Mutex<u32>) {
    let _guard = m.lock();
}
"#,
    );
    let found = findings(&out, "nonblocking");
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("calls `std::sync::Mutex::lock`, which waits for the lock"), "{out}");
}

/// A syscall that waits (`read`) is in the blocking set by its symbol.
#[test]
fn nonblocking_refuses_a_blocking_syscall() {
    let out = effects_of(
        "",
        r#"
unsafe extern "C" {
    fn read(fd: i32, buf: *mut u8, len: usize) -> isize;
}
#[blaze::nonblocking]
pub fn input(buf: &mut [u8; 8]) -> isize {
    unsafe { read(0, buf as *mut [u8; 8] as *mut u8, 8) }
}
"#,
    );
    let found = findings(&out, "nonblocking");
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("`read`"), "{out}");
}

// trusted, traits, malformed

/// A callee marked `trusted` takes its effect off the call: the caller passes, and the trusted
/// function itself is not checked.
#[test]
fn a_trusted_callee_is_taken_at_its_word() {
    let out = effects_of(
        "",
        r#"
#[blaze::nonblocking(trusted = "only ever uncontended")]
fn take(m: &std::sync::Mutex<u32>) {
    let _guard = m.lock();
}
#[blaze::nonblocking]
pub fn step(m: &std::sync::Mutex<u32>) {
    take(m)
}
"#,
    );
    assert_eq!(findings(&out, "nonblocking"), Vec::<&str>::new(), "{out}");
}

/// An attribute on a trait checks every impl's method: the one that allocates is refused.
#[test]
fn a_marked_trait_checks_every_impl() {
    let out = effects_of(
        "",
        r#"
#[blaze::noalloc]
pub trait Sink {
    fn put(&self, x: u32) -> u32;
}
pub struct Quiet;
impl Sink for Quiet {
    fn put(&self, x: u32) -> u32 {
        x
    }
}
pub struct Loud;
impl Sink for Loud {
    fn put(&self, x: u32) -> u32 {
        let _ = alloc::vec::Vec::<u32>::with_capacity(4);
        x
    }
}
"#,
    );
    let found = findings(&out, "noalloc");
    assert_eq!(found.len(), 1, "{out}");
    assert!(out.contains("lib.rs:13"), "`Loud`'s `put`:\n{out}");
}

/// blaze-fetch's `Transform` (crates/blaze-fetch/src/graph.rs) is `#[blaze::pure]` on a trait:
/// an impl whose `run` reads the clock or the environment is refused at its `run`, naming the
/// call; the impl that reads only its input passes. A fetch graph keys a step's output by its
/// input and code alone, which holds only while this refuses.
#[test]
fn an_impure_transform_is_refused() {
    let fixture = format!(
        "{MINICORE}{TOOLCHAIN}//- /src/lib.rs crate:main deps:alloc,std\n{}",
        r#"pub struct Tree(u32);
pub struct StepError;
#[blaze::pure]
pub trait Transform {
    type In;
    fn run(input: Self::In) -> Result<Tree, StepError>;
}
pub struct Same;
impl Transform for Same {
    type In = u32;
    fn run(input: u32) -> Result<Tree, StepError> {
        Ok(Tree(input))
    }
}
pub struct Stamped;
impl Transform for Stamped {
    type In = u32;
    fn run(input: u32) -> Result<Tree, StepError> {
        let _ = std::time::SystemTime::now();
        Ok(Tree(input))
    }
}
pub struct Configured;
impl Transform for Configured {
    type In = u32;
    fn run(input: u32) -> Result<Tree, StepError> {
        match std::env::var("STEP") {
            Ok(it) => Ok(Tree(it + input)),
            Err(_) => Err(StepError),
        }
    }
}
"#
    );
    let out = lint(&fixture, "-A clippy::all --deny=blaze::pure");
    let found = findings(&out, "pure");
    assert_eq!(found.len(), 2, "{out}");
    assert!(out.contains("lib.rs:18"), "`Stamped`'s `run`:\n{out}");
    assert!(out.contains("reads the clock"), "{out}");
    assert!(out.contains("lib.rs:26"), "`Configured`'s `run`:\n{out}");
    assert!(out.contains("reads the environment"), "{out}");
    assert!(!out.contains("lib.rs:11"), "`Same`'s `run` reads only its input:\n{out}");
}

/// An attribute with anything but `trusted = ".."` is a finding, never read as unmarked.
#[test]
fn a_malformed_attribute_is_refused() {
    let out = effects_of(
        "",
        r#"
#[blaze::nopanic(because)]
pub fn f() {}
"#,
    );
    assert!(out.contains("the attribute takes nothing, or `trusted = \"why\"`"), "{out}");
}
