//! One test per general speed lint (`bounds_check_in_loop`, `unbuffered_io`,
//! `stdout_lock_in_loop`, `front_vec_ops`, `missing_size_hint`, `fmt_allocates`, `jagged_vec`,
//! `stable_sort_needless`, `dyn_in_hot_field`), over an `std` of their own: a `Vec` and a
//! `String` known by their diagnostic items, a `Box`, `fs::File`, `io::_print` behind a
//! `println!`, and a `fmt` with `Display` and a `Formatter` that has `write_str`.

use crate::tests::lint;

const MINICORE: &str = "//- minicore: sized, copy, clone, option, result, fn, iterator, iterators, slice, deref, drop, index, builtin_impls, range, derive, eq, ord\n";

const STD: &str = r#"
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 {
        pub use crate::vec::Vec;
        pub use crate::string::String;
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod boxed {
    pub struct Box<T: ?Sized> { ptr: *const T }
    impl<T: ?Sized> core::ops::Deref for Box<T> {
        type Target = T;
        fn deref(&self) -> &T { loop {} }
    }
}
pub mod string {
    #[rustc_diagnostic_item = "String"]
    pub struct String { ptr: *const u8, cap: usize, len: usize }
    impl String {
        pub fn from(text: &str) -> String { loop {} }
    }
    impl core::ops::Deref for String {
        type Target = str;
        fn deref(&self) -> &str { loop {} }
    }
}
pub mod vec {
    #[rustc_diagnostic_item = "Vec"]
    pub struct Vec<T> { ptr: *const T, cap: usize, len: usize }
    impl<T> Vec<T> {
        pub const fn new() -> Vec<T> { loop {} }
        pub fn push(&mut self, value: T) {}
        pub fn len(&self) -> usize { self.len }
        pub fn is_empty(&self) -> bool { self.len == 0 }
        pub fn remove(&mut self, index: usize) -> T { loop {} }
        pub fn insert(&mut self, index: usize, value: T) {}
        pub fn sort(&mut self) where T: core::cmp::Ord {}
        pub fn sort_by_key<K: core::cmp::Ord, F: FnMut(&T) -> K>(&mut self, f: F) {}
    }
}
pub mod io {
    pub struct Error;
    pub type Result<T> = core::result::Result<T, Error>;
    pub fn _print(text: &str) {}
}
pub mod fs {
    pub struct File { fd: i32 }
    impl File {
        pub fn write_all(&mut self, bytes: &[u8]) -> crate::io::Result<()> { Ok(()) }
    }
}
pub mod fmt {
    pub struct Error;
    pub type Result = core::result::Result<(), Error>;
    pub struct Formatter<'a> { out: &'a () }
    impl Formatter<'_> {
        pub fn write_str(&mut self, text: &str) -> Result { Ok(()) }
    }
    pub trait Display {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result;
    }
}
#[macro_export]
macro_rules! println {
    ($text:literal) => { $crate::io::_print($text) };
}
"#;

const MAIN: &str = "//- /src/lib.rs crate:main deps:std\n";

/// What `blaze::<lint_name>` alone says of `main`, over [`STD`].
fn only(lint_name: &str, main: &str) -> String {
    let fixture = format!("{MINICORE}{STD}{MAIN}{}", main.trim_start_matches('\n'));
    lint(&fixture, &format!("-A clippy::all --warn=blaze::{lint_name}"))
}

/// A range loop not bounded by the slice's length is found; one up to `xs.len()` is not.
#[test]
fn bounds_check_in_loop_finds_an_index_the_loop_bound_does_not_prove() {
    let found = only(
        "bounds_check_in_loop",
        r#"
pub fn sum(xs: &[u32], n: usize) -> u32 {
    let mut total = 0;
    for i in 0..n {
        total += xs[i];
    }
    total
}
pub fn sum_all(xs: &[u32]) -> u32 {
    let mut total = 0;
    for i in 0..xs.len() {
        total += xs[i];
    }
    total
}
"#,
    );
    assert_eq!(found.matches("the bounds check stays in every iteration").count(), 1, "{found}");
    assert!(
        found.contains("`xs` is indexed by the loop variable of a range ending at `n`"),
        "{found}"
    );
}

/// Two writes to one `File` are found; one write is not.
#[test]
fn unbuffered_io_finds_repeated_writes_to_a_file() {
    let found = only(
        "unbuffered_io",
        r#"
use std::fs::File;
pub fn save(file: &mut File) {
    let _ = file.write_all(b"head");
    let _ = file.write_all(b"body");
}
pub fn once(file: &mut File) {
    let _ = file.write_all(b"all");
}
"#,
    );
    assert_eq!(found.matches("with no `BufWriter`").count(), 1, "{found}");
    assert!(found.contains("`file` is a `File` written 2 times"), "{found}");
}

/// Two prints in one loop are one finding; a print outside a loop is none.
#[test]
fn stdout_lock_in_loop_finds_prints_in_a_loop_once_per_loop() {
    let found = only(
        "stdout_lock_in_loop",
        r#"
pub fn show(n: u32) {
    let mut i = 0;
    while i < n {
        std::println!("a");
        std::println!("b");
        i += 1;
    }
}
pub fn once() {
    std::println!("x");
}
"#,
    );
    assert_eq!(found.matches("is inside a loop").count(), 1, "{found}");
    assert!(found.contains("(and 1 more in the same loop)"), "{found}");
}

/// `remove(0)` in a loop that also pushes is found as a queue; one outside a loop is not.
#[test]
fn front_vec_ops_finds_a_vec_used_as_a_queue() {
    let found = only(
        "front_vec_ops",
        r#"
pub fn rotate(queue: &mut Vec<u32>, n: u32) {
    let mut i = 0;
    while i < n {
        let x = queue.remove(0);
        queue.push(x);
        i += 1;
    }
}
pub fn first(v: &mut Vec<u32>) -> u32 {
    v.remove(0)
}
"#,
    );
    assert_eq!(
        found.matches("on a `Vec` inside a loop shifts every other element").count(),
        1,
        "{found}"
    );
    assert!(found.contains("it is used as a queue"), "{found}");
}

/// An iterator stepping an index over a `Vec` with no `size_hint` is found; one over nothing is
/// not.
#[test]
fn missing_size_hint_finds_a_counter_over_a_vec() {
    let found = only(
        "missing_size_hint",
        r#"
pub struct Items { items: Vec<u32>, at: usize }
impl core::iter::Iterator for Items {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        self.at += 1;
        None
    }
}
pub struct Forever { at: usize }
impl core::iter::Iterator for Forever {
    type Item = usize;
    fn next(&mut self) -> Option<usize> {
        self.at += 1;
        Some(self.at)
    }
}
"#,
    );
    assert_eq!(found.matches("has no `size_hint`").count(), 1, "{found}");
    assert!(found.contains("`impl Iterator for Items`"), "{found}");
}

/// A `String` built only to be written to the formatter is found; one that is also measured is
/// not.
#[test]
fn fmt_allocates_finds_a_string_only_written_to_the_formatter() {
    let found = only(
        "fmt_allocates",
        r#"
use std::fmt::{Display, Formatter, Result};
pub struct Name;
impl Display for Name {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let text = String::from("name");
        f.write_str(&text)
    }
}
pub struct Kept;
impl Display for Kept {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let text = String::from("kept");
        let _ = text.len();
        f.write_str(&text)
    }
}
"#,
    );
    assert_eq!(found.matches("only to write it to the formatter").count(), 1, "{found}");
}

/// A `Vec<Vec<u32>>` field nothing changes is found; one pushed to is not.
#[test]
fn jagged_vec_finds_a_frozen_vec_of_vecs() {
    let found = only(
        "jagged_vec",
        r#"
pub struct Table { rows: Vec<Vec<u32>> }
impl Table {
    pub fn new(rows: Vec<Vec<u32>>) -> Table { Table { rows } }
    pub fn count(&self) -> usize { self.rows.len() }
}
pub struct Growing { rows: Vec<Vec<u32>> }
impl Growing {
    pub fn add(&mut self, row: Vec<u32>) { self.rows.push(row) }
}
"#,
    );
    assert_eq!(found.matches("one heap allocation per").count(), 1, "{found}");
    assert!(found.contains("the field `rows` of `Table`"), "{found}");
}

/// `sort()` of a derived-`Ord` struct and `sort_by_key(|x| *x)` are found; `sort()` of
/// integers is clippy's `stable_sort_primitive` and is not.
#[test]
fn stable_sort_needless_finds_sorts_of_indistinguishable_elements() {
    let found = only(
        "stable_sort_needless",
        r#"
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub struct Name { text: String, id: u32 }
pub fn names(names: &mut Vec<Name>) { names.sort(); }
pub fn ints(xs: &mut Vec<u32>) { xs.sort(); }
pub fn keyed(xs: &mut Vec<u32>) { xs.sort_by_key(|x| *x); }
"#,
    );
    assert_eq!(found.matches("equal elements here can't be told apart").count(), 2, "{found}");
    assert!(found.contains("`sort_unstable` gives the same order"), "{found}");
    assert!(found.contains("`sort_unstable_by_key` gives the same order"), "{found}");
}

/// A boxed closure a method calls in a loop is found; one in a struct kept in a slice is not.
#[test]
fn dyn_in_hot_field_finds_a_trait_object_called_in_a_loop() {
    let found = only(
        "dyn_in_hot_field",
        r#"
use std::boxed::Box;
struct Mapper { f: Box<dyn Fn(u32) -> u32> }
impl Mapper {
    fn run(&self, n: u32) -> u32 {
        let mut i = 0;
        let mut total = 0;
        while i < n {
            total += (self.f)(i);
            i += 1;
        }
        total
    }
}
struct Stage { f: Box<dyn Fn(u32) -> u32> }
impl Stage {
    fn run(&self, n: u32) -> u32 {
        let mut i = 0;
        let mut total = 0;
        while i < n {
            total += (self.f)(i);
            i += 1;
        }
        total
    }
}
fn stages(all: &[Stage]) -> usize { 0 }
"#,
    );
    assert_eq!(found.matches("every call goes through the vtable").count(), 1, "{found}");
    assert!(found.contains("the field `f` of `Mapper`"), "{found}");
    assert!(found.contains("trades") || found.contains("one copy of the struct's code"), "{found}");
}
