//! One test for each of the general correctness lints (`blocking_in_async`, `relaxed_publish`,
//! `unwind_across_ffi`, `unbounded_from_input`, `dos_hasher_on_input`, `panic_in_drop`), over
//! an `std` with the few items they look for, and one for `dos_hasher_on_input` yielding to
//! `blaze::internal`.

use crate::{
    level::{CommandLine, Level, Levels, Lint as Asked},
    tests::lint,
};

const MINICORE: &str = "//- minicore: sized, copy, clone, option, result, fn, iterator, iterators, slice, deref, drop, index, future, builtin_impls\n";

const CRATES: &str = r#"
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 {
        pub use crate::{string::String, vec::Vec};
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod fs {
    pub fn read(path: &str) -> u8 { loop {} }
}
pub mod thread {
    pub fn sleep(ms: u64) {}
    pub fn panicking() -> bool { false }
}
pub mod io {
    pub struct Lines<B> { b: B }
    impl<B> core::iter::Iterator for Lines<B> {
        type Item = u8;
        fn next(&mut self) -> Option<u8> { loop {} }
    }
}
pub mod sync {
    pub mod atomic {
        pub enum Ordering { Relaxed, Release, Acquire, AcqRel, SeqCst }
        pub struct AtomicBool { v: u8 }
        impl AtomicBool {
            pub const fn new(v: bool) -> AtomicBool { AtomicBool { v: 0 } }
            pub fn load(&self, order: Ordering) -> bool { loop {} }
            pub fn store(&self, v: bool, order: Ordering) {}
        }
    }
}
pub mod hash {
    pub struct BuildHasherDefault<H> { h: H }
}
pub mod collections {
    pub struct HashMap<K, V, S> { k: K, v: V, s: S }
    impl<K, V, S> HashMap<K, V, S> {
        pub fn insert(&mut self, k: K, v: V) -> Option<V> { None }
    }
}
pub mod string {
    #[rustc_diagnostic_item = "String"]
    pub struct String { v: u8 }
}
pub mod vec {
    #[rustc_diagnostic_item = "Vec"]
    pub struct Vec<T> { ptr: *const T, len: usize }
    impl<T> Vec<T> {
        pub fn push(&mut self, value: T) {}
        pub fn len(&self) -> usize { self.len }
    }
}
"#;

/// What `blaze::<name>` alone says of the crate `main` written `main`.
fn only(name: &str, main: &str) -> String {
    let fixture = format!(
        "{MINICORE}{CRATES}//- /src/lib.rs crate:main deps:std\n{}",
        main.trim_start_matches('\n')
    );
    lint(&fixture, &format!("-A clippy::all --warn=blaze::{name}"))
}

#[test]
fn blocking_in_async_finds_a_blocking_call_in_an_async_fn_only() {
    let out = only(
        "blocking_in_async",
        r#"
pub async fn load(path: &str) -> u8 {
    std::thread::sleep(10);
    std::fs::read(path)
}
pub fn load_now(path: &str) -> u8 {
    std::fs::read(path)
}
"#,
    );
    assert_eq!(out.matches("blocks the thread inside this").count(), 2, "{out}");
    assert!(out.contains("`std::fs::read`"), "{out}");
}

#[test]
fn relaxed_publish_finds_a_relaxed_flag_that_guards_data() {
    let out = only(
        "relaxed_publish",
        r#"
use std::sync::atomic::{AtomicBool, Ordering};
pub struct Slot { ready: AtomicBool, value: u64 }
impl Slot {
    pub fn publish(&mut self, v: u64) {
        self.value = v;
        self.ready.store(true, Ordering::Relaxed);
    }
    pub fn read(&self) -> u64 {
        if self.ready.load(Ordering::Acquire) { self.value } else { 0 }
    }
}
pub struct Counted { done: AtomicBool }
impl Counted {
    pub fn finish(&self) { self.done.store(true, Ordering::Relaxed); }
    pub fn is_done(&self) -> bool { self.done.load(Ordering::Relaxed) }
}
"#,
    );
    assert_eq!(
        out.matches("this `Relaxed` store of `Slot::ready` publishes it").count(),
        1,
        "{out}"
    );
    assert_eq!(out.matches("this load of `Slot::ready` guards other data").count(), 1, "{out}");
    assert!(!out.contains("Counted::done"), "a flag that guards nothing is fine:\n{out}");
}

#[test]
fn unwind_across_ffi_finds_a_panic_in_extern_c_but_not_c_unwind() {
    let out = only(
        "unwind_across_ffi",
        r#"
pub extern "C" fn at(xs: &[u8], i: usize) -> u8 { xs[i] }
pub extern "C-unwind" fn at_unwind(xs: &[u8], i: usize) -> u8 { xs[i] }
pub fn at_rust(xs: &[u8], i: usize) -> u8 { xs[i] }
"#,
    );
    assert_eq!(out.matches("can panic inside `extern").count(), 1, "{out}");
    assert!(out.contains("this index can panic inside `extern \"C\" fn at`"), "{out}");
}

#[test]
fn unbounded_from_input_finds_growth_with_no_cap() {
    let out = only(
        "unbounded_from_input",
        r#"
pub fn keep(lines: std::io::Lines<u8>, out: &mut Vec<u8>) {
    for line in lines {
        out.push(line);
    }
}
pub fn keep_some(lines: std::io::Lines<u8>, out: &mut Vec<u8>) {
    for line in lines {
        if out.len() > 64 { break; }
        out.push(line);
    }
}
"#,
    );
    assert_eq!(
        out.matches("grows a collection once per round of a loop that reads input").count(),
        1,
        "{out}"
    );
}

#[test]
fn dos_hasher_on_input_finds_a_fixed_hasher_keyed_by_a_parameter() {
    let out = only(
        "dos_hasher_on_input",
        r#"
use std::{collections::HashMap, hash::BuildHasherDefault};
pub struct FxHasher;
pub fn count(map: &mut HashMap<String, u32, BuildHasherDefault<FxHasher>>, word: String) {
    map.insert(word, 1);
}
pub fn count_one(map: &mut HashMap<u32, u32, BuildHasherDefault<FxHasher>>, n: u32) {
    map.insert(n, 1);
}
fn count_private(map: &mut HashMap<String, u32, BuildHasherDefault<FxHasher>>, word: String) {
    map.insert(word, 1);
}
"#,
    );
    assert_eq!(out.matches("with a fixed-seed hasher").count(), 1, "{out}");
    assert!(out.contains("by text from `word`, a parameter of `pub fn count`"), "{out}");
}

#[test]
fn dos_hasher_on_input_yields_to_blaze_internal() {
    let level = |flags: &[&str]| {
        let args: Vec<String> = flags.iter().map(|it| (*it).to_owned()).collect(); // alloc: a test's flags
        let levels = Levels::new(&CommandLine::parse(&args).unwrap());
        levels.of(Asked::Blaze(super::Lint::DosHasherOnInput), &[]).level
    };
    assert_eq!(level(&[]), Level::Warn);
    assert_eq!(level(&["--warn=blaze::internal"]), Level::Allow);
    assert_eq!(
        level(&["--warn=blaze::internal", "--warn=blaze::dos_hasher_on_input"]),
        Level::Warn
    );
    assert_eq!(level(&["--allow=blaze::internal"]), Level::Warn);
    // A lint that doesn't yield keeps its default.
    let levels = Levels::new(&CommandLine::parse(&["--warn=blaze::internal".to_owned()]).unwrap());
    assert_eq!(levels.of(Asked::Blaze(super::Lint::PanicInDrop), &[]).level, Level::Warn);
}

#[test]
fn panic_in_drop_finds_an_unwrap_unless_it_checks_panicking() {
    let out = only(
        "panic_in_drop",
        r#"
pub struct Guard { v: Option<u8> }
impl Drop for Guard {
    fn drop(&mut self) { self.v.unwrap(); }
}
pub struct Careful { v: Option<u8> }
impl Drop for Careful {
    fn drop(&mut self) {
        if std::thread::panicking() { return; }
        self.v.unwrap();
    }
}
"#,
    );
    assert_eq!(out.matches("can panic in `Drop::drop`").count(), 1, "{out}");
    assert!(out.contains("this `unwrap` can panic in `Drop::drop` of `Guard`"), "{out}");
}
