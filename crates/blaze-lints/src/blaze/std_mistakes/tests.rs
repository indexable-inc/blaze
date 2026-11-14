//! The std-mistake lints over an `alloc` and a `std` with the shapes they look at, each alone on
//! the command line: where each fires, where it does not, what its fix writes and that the
//! fixed file lints clean and fixes to itself.
//!
//! minicore's `core` has no float `PartialOrd`, no `str` methods past `as_bytes` and no
//! `RefCell`: the fixture's `std` gives the floats their comparison, `to_bits`, `from_bits`
//! and `total_cmp`, `u32` its `to_ne_bytes`, `str` its `split_at`, `find`, `get` and `len` (as
//! `#[rustc_allow_incoherent_impl]` methods, as `alloc` does for the real `str`),
//! `ManuallyDrop` its `into_inner`, and `RefCell` its diagnostic item. `from` gives `Result` its
//! `Try` (a fix that writes `?` type-checks), `int_cast` the `From` and `cast_signed` the
//! `transmute` fixes write, and `expect` the `expect` of `Option` and `Result`.

use crate::{conf::Conf, tests::lint};

const MINICORE: &str = "//- minicore: sized, copy, clone, option, result, fn, iterator, iterators, iterators_more, slice, index, range, deref, drop, manually_drop, transmute, ord, eq, builtin_impls, try, from, int_cast, expect, str, fmt, panic, assert\n";

const CRATES: &str = r#"
//- /alloc/lib.rs crate:alloc
pub mod vec {
    pub struct Vec<T> { ptr: *const T, len: usize }
    impl<T> Vec<T> {
        pub const fn new() -> Vec<T> { loop {} }
        pub fn len(&self) -> usize { self.len }
        pub fn push(&mut self, value: T) {}
    }
    impl<T: Clone> Clone for Vec<T> {
        fn clone(&self) -> Vec<T> { loop {} }
    }
}
pub mod string {
    pub struct String { vec: crate::vec::Vec<u8> }
    impl String {
        pub const fn new() -> String { loop {} }
        pub fn len(&self) -> usize { 0 }
    }
    impl Clone for String {
        fn clone(&self) -> String { loop {} }
    }
    impl core::ops::Deref for String {
        type Target = str;
        fn deref(&self) -> &str { loop {} }
    }
}
pub mod boxed {
    pub struct Box<T: ?Sized> { ptr: *const T }
    impl<T> Box<T> {
        pub fn new(value: T) -> Box<T> { loop {} }
    }
    impl<T: ?Sized> Box<T> {
        pub fn leak<'a>(b: Box<T>) -> &'a mut T { loop {} }
    }
}
pub mod collections {
    pub struct VecDeque<T> { t: T }
    pub struct BTreeMap<K, V> { k: K, v: V }
    pub struct BTreeSet<T> { t: T }
}
pub mod str {
    impl str {
        #[rustc_allow_incoherent_impl]
        pub fn len(&self) -> usize { 0 }
        #[rustc_allow_incoherent_impl]
        pub fn find(&self, c: char) -> Option<usize> { None }
        #[rustc_allow_incoherent_impl]
        pub fn get(&self, r: core::ops::Range<usize>) -> Option<&str> { None }
        #[rustc_allow_incoherent_impl]
        pub fn split_at(&self, mid: usize) -> (&str, &str) { loop {} }
        #[rustc_allow_incoherent_impl]
        pub fn split_at_checked(&self, mid: usize) -> Option<(&str, &str)> { None }
        #[rustc_allow_incoherent_impl]
        pub fn is_char_boundary(&self, index: usize) -> bool { false }
    }
}
//- /std/lib.rs crate:std deps:alloc
pub mod prelude {
    pub mod v1 {
        pub use alloc::{boxed::Box, string::String, vec::Vec};
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub use alloc::{boxed, string, vec};
pub use core::{cmp, fmt};
pub mod mem {
    pub use core::mem::*;
    impl<T> ManuallyDrop<T> {
        #[rustc_allow_incoherent_impl]
        pub fn into_inner(slot: ManuallyDrop<T>) -> T { loop {} }
    }
}
pub mod floats {
    impl core::cmp::PartialEq for f64 {
        fn eq(&self, other: &f64) -> bool { false }
    }
    impl core::cmp::PartialOrd for f64 {
        fn partial_cmp(&self, other: &f64) -> Option<core::cmp::Ordering> { None }
    }
    impl f32 {
        #[rustc_allow_incoherent_impl]
        pub const fn to_bits(self) -> u32 { loop {} }
        #[rustc_allow_incoherent_impl]
        pub const fn from_bits(v: u32) -> f32 { loop {} }
    }
    impl f64 {
        #[rustc_allow_incoherent_impl]
        pub const fn to_bits(self) -> u64 { loop {} }
        #[rustc_allow_incoherent_impl]
        pub const fn from_bits(v: u64) -> f64 { loop {} }
        #[rustc_allow_incoherent_impl]
        pub fn total_cmp(&self, other: &f64) -> core::cmp::Ordering { loop {} }
    }
    impl u32 {
        #[rustc_allow_incoherent_impl]
        pub const fn to_ne_bytes(self) -> [u8; 4] { loop {} }
    }
}
pub mod io {
    pub struct Error;
    pub type Result<T> = core::result::Result<T, Error>;
    pub trait Write {
        fn write(&mut self, buf: &[u8]) -> Result<usize>;
        fn write_all(&mut self, buf: &[u8]) -> Result<()> { Ok(()) }
    }
    pub trait Read {
        fn read_to_end(&mut self, buf: &mut crate::vec::Vec<u8>) -> Result<usize> { Ok(0) }
        fn take(self, limit: u64) -> Take<Self> where Self: Sized { Take { inner: self, limit } }
    }
    pub trait BufRead {
        fn read_line(&mut self, buf: &mut crate::string::String) -> Result<usize> { Ok(0) }
    }
    pub struct Take<R> { inner: R, limit: u64 }
    impl<R: Read> Read for Take<R> {}
    pub struct Stdin;
    pub struct Sink;
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> Result<usize> { Ok(0) }
    }
}
pub mod fs {
    pub fn read(path: &str) -> crate::io::Result<crate::vec::Vec<u8>> { loop {} }
    pub fn read_to_string(path: &str) -> crate::io::Result<crate::string::String> { loop {} }
}
pub mod process {
    pub struct Command;
    pub struct Output;
    impl Command {
        pub fn new(program: &str) -> Command { Command }
        pub fn output(&mut self) -> crate::io::Result<Output> { loop {} }
    }
    pub fn exit(code: i32) -> ! { loop {} }
    pub fn abort() -> ! { loop {} }
}
pub mod sync {
    pub struct PoisonError<T> { guard: T }
    pub type LockResult<T> = core::result::Result<T, PoisonError<T>>;
    pub struct Mutex<T> { value: T }
    pub struct MutexGuard<'a, T> { value: &'a T }
    impl<T> Mutex<T> {
        pub const fn new(value: T) -> Mutex<T> { Mutex { value } }
        pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> { loop {} }
        pub fn try_lock(&self) -> LockResult<MutexGuard<'_, T>> { loop {} }
    }
    pub struct RwLock<T> { value: T }
    impl<T> RwLock<T> {
        pub fn read(&self) -> LockResult<&T> { loop {} }
        pub fn write(&self) -> LockResult<&mut T> { loop {} }
    }
}
#[macro_export]
macro_rules! println { ($($t:tt)*) => { () }; }
#[macro_export]
macro_rules! eprintln { ($($t:tt)*) => { () }; }
#[macro_export]
macro_rules! format { ($($t:tt)*) => { $crate::string::String::new() }; }
pub mod thread {
    pub struct JoinHandle<T> { t: T }
    impl<T> JoinHandle<T> {
        pub fn join(self) -> core::result::Result<T, ()> { loop {} }
    }
    pub fn spawn<F: FnOnce() -> T, T>(f: F) -> JoinHandle<T> { loop {} }
    pub struct Builder;
    impl Builder {
        pub fn new() -> Builder { Builder }
        pub fn spawn<F: FnOnce() -> T, T>(self, f: F) -> crate::io::Result<JoinHandle<T>> { loop {} }
    }
}
pub mod path {
    pub struct Path { inner: [u8] }
    impl Path {
        pub fn to_str(&self) -> Option<&str> { None }
        pub fn to_string_lossy(&self) -> crate::string::String { loop {} }
    }
    pub struct PathBuf;
}
pub mod ffi {
    pub struct OsStr { inner: [u8] }
    impl OsStr {
        pub fn to_str(&self) -> Option<&str> { None }
    }
    pub struct OsString;
}
pub mod collections {
    pub use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
    pub struct HashMap<K, V> { k: K, v: V }
    pub struct HashSet<T> { t: T }
}
pub mod cell {
    #[rustc_diagnostic_item = "RefCell"]
    pub struct RefCell<T> { value: T }
    impl<T> RefCell<T> {
        pub const fn new(value: T) -> RefCell<T> { RefCell { value } }
        pub fn borrow(&self) -> Ref<'_, T> { loop {} }
        pub fn borrow_mut(&self) -> RefMut<'_, T> { loop {} }
    }
    pub struct Ref<'a, T> { value: &'a T }
    pub struct RefMut<'a, T> { value: &'a mut T }
}
//- /crossbeam_channel/lib.rs crate:crossbeam_channel
pub struct Sender<T> { t: T }
pub struct Receiver<T> { t: T }
pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) { loop {} }
pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>) { loop {} }
"#;

const MAIN: &str = "//- /src/lib.rs crate:main deps:std,alloc,crossbeam_channel\n";

fn fixture_of(main: &str) -> String {
    format!("{MINICORE}{CRATES}{MAIN}{}", main.trim_start_matches('\n'))
}

fn flags(lint_name: &str) -> String {
    format!("-A clippy::all --warn=blaze::{lint_name}")
}

/// What `blaze::<lint_name>` alone says of `main`.
fn found(lint_name: &str, main: &str) -> String {
    lint(&fixture_of(main), &flags(lint_name))
}

/// What `blaze::<lint_name>` alone says of `main` as the `src/main.rs` of a binary crate, whose
/// crate root is its entry module.
fn found_in_bin(lint_name: &str, main: &str) -> String {
    let fixture = format!(
        "{MINICORE}{CRATES}//- /src/main.rs crate:bin:main deps:std,alloc,crossbeam_channel\n{}",
        main.trim_start_matches('\n')
    );
    lint(&fixture, &flags(lint_name))
}

/// `src/lib.rs` of `main` after `blaze check --fix` with `blaze::<lint_name>` alone on, after
/// checking that the fix changed it, that linting it again finds nothing, and that fixing it
/// again changes nothing.
fn fixed(lint_name: &str, main: &str) -> String {
    crate::tests::fixed_in(&fixture_of(main), &flags(lint_name))
}

/// `main` after every suggestion `blaze::<lint_name>` offers is made, a `MaybeIncorrect` one
/// too (which `blaze check --fix` leaves to the author), after checking that the suggestions
/// changed it, that linting it again finds nothing, and that it offers nothing more, so
/// making the suggestions again changes nothing.
fn offered(lint_name: &str, main: &str) -> String {
    crate::tests::offered_in(&fixture_of(""), main.trim_start_matches('\n'), &flags(lint_name))
}

#[test]
fn partial_write_finds_a_dropped_count_and_its_fix_writes_all() {
    const DROPPED: &str = r#"
use std::io::{Sink, Write};
pub fn dropped(w: &mut Sink, b: &[u8]) -> std::io::Result<()> {
    w.write(b)?;
    let _ = w.write(b);
    Ok(())
}
"#;
    let out = found(
        "partial_write",
        &format!(
            "{DROPPED}{}",
            r#"
pub fn counted(w: &mut Sink, b: &[u8]) -> std::io::Result<usize> {
    let n = w.write(b)?;
    Ok(n)
}
"#
        ),
    );
    assert_eq!(out.matches("the count `Write::write` returns is thrown away").count(), 2, "{out}");
    let after = fixed("partial_write", DROPPED);
    assert!(after.contains("    w.write_all(b)?;\n    let _ = w.write_all(b);\n"), "{after}");
}

#[test]
fn partial_cmp_unwrap_finds_floats_only_and_offers_total_cmp() {
    let out = found(
        "partial_cmp_unwrap",
        r#"
use std::cmp::{Ordering, PartialOrd};
pub fn floats(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap()
}
pub fn ints(a: u32, b: u32) -> Ordering {
    a.partial_cmp(&b).unwrap()
}
pub fn kept(a: f64, b: f64) -> Option<Ordering> {
    a.partial_cmp(&b)
}
"#,
    );
    assert_eq!(out.matches("`partial_cmp` of two `f64`s is `None`").count(), 1, "{out}");
    assert!(out.contains("a.total_cmp(&b)"), "the fix is offered:\n{out}");
    let after = offered(
        "partial_cmp_unwrap",
        r#"
use std::cmp::{Ordering, PartialOrd};
pub fn floats(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap()
}
pub fn negated(a: f64, b: f64) -> Ordering {
    (-a).partial_cmp(&b).expect("no NaN")
}
"#,
    );
    assert!(after.contains("    a.total_cmp(&b)\n"), "{after}");
    assert!(after.contains("    (-a).total_cmp(&b)\n"), "{after}");
}

#[test]
fn unbounded_read_finds_reads_with_no_limit_and_not_through_take() {
    let out = found(
        "unbounded_read",
        r#"
use std::io::{BufRead, Read};
pub fn all<R: Read>(mut r: R, buf: &mut Vec<u8>) {
    let _n = r.read_to_end(buf);
}
pub fn capped<R: Read>(r: R, buf: &mut Vec<u8>) {
    let _n = r.take(16).read_to_end(buf);
}
pub fn file() {
    let _b = std::fs::read("x");
}
pub fn child() {
    let _o = std::process::Command::new("ls").output();
}
pub fn line<B: BufRead>(mut r: B, s: &mut String) {
    let _n = r.read_line(s);
}
"#,
    );
    assert_eq!(out.matches("reads until the input ends").count(), 1, "{out}");
    assert_eq!(out.matches("reads the whole file").count(), 1, "{out}");
    assert_eq!(out.matches("keeps everything the child writes").count(), 1, "{out}");
    assert_eq!(out.matches("reads until a newline").count(), 1, "{out}");
}

#[test]
fn len_minus_finds_an_unchecked_length_minus_and_offers_checked_sub_in_an_option() {
    const OPTION: &str = r#"
pub fn opt(v: &[u32], n: usize) -> Option<usize> {
    Some(v.len() - n)
}
"#;
    let out = found(
        "len_minus",
        &format!(
            "{OPTION}{}",
            r#"
pub fn last(v: &[u32]) -> usize {
    v.len() - 1
}
pub fn guarded(v: &[u32]) -> usize {
    if v.len() > 0 { v.len() - 1 } else { 0 }
}
pub fn signed(a: i64, len: i64) -> i64 {
    len - a
}
pub fn zero(v: &[u32]) -> usize {
    v.len() - 0
}
pub fn early(v: &[u32], n: usize) -> usize {
    if n > v.len() {
        return 0;
    }
    v.len() - n
}
pub fn asserted(v: &[u32], n: usize) -> usize {
    assert!(n <= v.len());
    v.len() - n
}
pub fn other(v: &[u32], w: &[u32], n: usize) -> usize {
    if n <= w.len() { v.len() - n } else { 0 }
}
pub fn local(v: &[u32]) -> usize {
    let n = v.len();
    n - 1
}
pub fn local_guarded(v: &[u32]) -> usize {
    let n = v.len();
    if v.len() > 0 { n - 1 } else { 0 }
}
pub fn named(total_len: usize, n: usize) -> usize {
    total_len - n
}
pub struct Own;
impl Own {
    pub fn len(&self) -> usize { 3 }
}
pub fn own(o: &Own) -> usize {
    o.len() - 1
}
"#
        ),
    );
    // `last`, `opt`, `other` (its check is of another slice) and `local`; a name that says
    // length (`named`) and a `len` that is not std's (`own`) are no length to it.
    assert_eq!(out.matches("underflows when").count(), 4, "{out}");
    assert!(out.contains("`v.len() - n` underflows"), "{out}");
    assert!(out.contains("`n - 1` underflows"), "{out}");
    assert!(!out.contains("`total_len - n`") && !out.contains("`o.len() - 1`"), "{out}");
    let after = offered("len_minus", OPTION);
    assert!(after.contains("Some(v.len().checked_sub(n)?)"), "{after}");
}

#[test]
fn str_byte_slice_finds_unproved_indices_and_its_fix_hands_a_none_on() {
    const FIXABLE: &str = r#"
pub fn between(s: &str, a: usize, b: usize) -> Option<&str> {
    Some(&s[a..b])
}
pub fn halves(s: &str, n: usize) -> Option<(&str, &str)> {
    Some(s.split_at(n))
}
"#;
    let out = found(
        "str_byte_slice",
        &format!(
            "{FIXABLE}{}",
            r#"
pub fn head(s: &str, n: usize) -> &str {
    &s[0..n]
}
pub fn constant(s: &str) -> &str {
    &s[0..3]
}
pub fn found_at(s: &str) -> &str {
    let at = s.find('x').unwrap();
    &s[0..at]
}
pub fn bounded(s: &str, n: usize) -> &str {
    if !s.is_char_boundary(n) {
        return "";
    }
    &s[..n]
}
pub fn asserted(s: &str, n: usize) -> (&str, &str) {
    assert!(s.is_char_boundary(n));
    s.split_at(n)
}
pub fn other_checked(s: &str, t: &str, n: usize) -> &str {
    if !t.is_char_boundary(n) {
        return "";
    }
    &s[..n]
}
pub fn commented(s: &str, n: usize) -> &str {
    // Nothing here calls s.is_char_boundary(n): a comment is no check.
    &s[..n]
}
"#
        ),
    );
    // `between`, `halves`, `head`, `other_checked` (its check is of another string) and
    // `commented`.
    assert_eq!(out.matches("at a byte index nothing says is on a character boundary").count(), 5, "{out}");
    let after = fixed("str_byte_slice", FIXABLE);
    assert!(after.contains("Some(s.get(a..b)?)"), "{after}");
    assert!(after.contains("Some(s.split_at_checked(n)?)"), "{after}");
}

#[test]
fn forget_finds_a_manually_drop_never_ended_and_a_leak_outside_a_static() {
    let out = found(
        "forget",
        r#"
use std::mem::ManuallyDrop;
pub fn thrown(v: Vec<u32>) {
    let _ = ManuallyDrop::new(v);
}
pub fn read(v: Vec<u32>) -> usize {
    let m = ManuallyDrop::new(v);
    m.len()
}
pub fn ended(v: Vec<u32>) -> Vec<u32> {
    let m = ManuallyDrop::new(v);
    ManuallyDrop::into_inner(m)
}
pub fn handed(v: Vec<u32>) -> ManuallyDrop<Vec<u32>> {
    let m = ManuallyDrop::new(v);
    m
}
pub fn leaked() -> &'static mut u32 {
    Box::leak(Box::new(1))
}
pub static ONCE: &u32 = Box::leak(Box::new(1));
"#,
    );
    assert_eq!(out.matches("this `ManuallyDrop` is never dropped").count(), 2, "{out}");
    assert_eq!(out.matches("`Box::leak` gives this memory up").count(), 1, "{out}");
}

#[test]
fn path_to_str_unwrap_finds_a_panic_and_a_silent_lossy_outside_display() {
    let out = found(
        "path_to_str_unwrap",
        r#"
use std::{ffi::OsStr, fmt, path::Path};
pub fn name(p: &Path) -> &str {
    p.to_str().unwrap()
}
pub fn os(p: &OsStr) -> &str {
    p.to_str().unwrap()
}
pub fn kept(p: &Path) -> Option<&str> {
    p.to_str()
}
pub fn lossy(p: &Path) -> String {
    p.to_string_lossy()
}
pub struct Shown<'a>(&'a Path);
impl fmt::Display for Shown<'_> {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _shown = self.0.to_string_lossy();
        Ok(())
    }
}
"#,
    );
    assert_eq!(out.matches("`Path::to_str` is `None`").count(), 1, "{out}");
    assert_eq!(out.matches("`OsStr::to_str` is `None`").count(), 1, "{out}");
    assert_eq!(out.matches("`Path::to_string_lossy` changes").count(), 1, "{out}");
}

#[test]
fn detached_thread_finds_a_handle_dropped_or_never_named() {
    let out = found(
        "detached_thread",
        r#"
use std::thread;
pub fn detached() {
    thread::spawn(|| {});
}
pub fn built() {
    let _h = thread::Builder::new().spawn(|| {});
}
pub fn unnamed() {
    let h = thread::spawn(|| 1);
}
pub fn joined() {
    let h = thread::spawn(|| {});
    let _ = h.join();
}
pub fn handed() -> thread::JoinHandle<()> {
    thread::spawn(|| {})
}
"#,
    );
    assert_eq!(out.matches("the `JoinHandle` of this").count(), 3, "{out}");
}

#[test]
fn transmute_finds_every_call_and_its_fix_writes_the_safe_function() {
    const FIXABLE: &str = r#"
pub fn bits(x: f32) -> u32 {
    unsafe { std::mem::transmute(x) }
}
pub fn float(x: u64) -> f64 {
    unsafe { std::mem::transmute::<u64, f64>(x) }
}
pub fn signed(x: u32) -> i32 {
    unsafe { std::mem::transmute(x) }
}
pub fn bytes(x: u32) -> [u8; 4] {
    unsafe { std::mem::transmute(x) }
}
pub fn flag(x: bool) -> u8 {
    unsafe { std::mem::transmute(x) }
}
"#;
    let out = found(
        "transmute",
        &format!(
            "{FIXABLE}{}",
            r#"
pub fn longer(x: &u32) -> &'static u32 {
    unsafe { std::mem::transmute(x) }
}
pub fn pointer(p: *const u32) -> *const u8 {
    unsafe { std::mem::transmute(p) }
}
"#
        ),
    );
    assert_eq!(out.matches("has a safe function in std that says the same").count(), 6, "{out}");
    assert_eq!(out.matches("checks nothing but the two types' sizes").count(), 1, "{out}");
    assert!(out.contains("p.cast()"), "the pointer's fix is offered:\n{out}");
    let after = fixed("transmute", FIXABLE);
    for written in ["x.to_bits()", "f64::from_bits(x)", "x.cast_signed()", "x.to_ne_bytes()", "u8::from(x)"]
    {
        assert!(after.contains(&format!("{{\n    {written}\n}}")), "{written}:\n{after}");
    }
}

#[test]
fn clone_to_satisfy_finds_a_clone_only_borrowed_and_its_fix_borrows() {
    const FIXABLE: &str = r#"
pub fn takes(s: &String) -> usize {
    s.len()
}
pub fn handed(s: String) -> usize {
    takes(&s.clone())
}
pub fn lent(s: &String) -> usize {
    takes(&s.clone())
}
"#;
    let out = found(
        "clone_to_satisfy",
        &format!(
            "{FIXABLE}{}",
            r#"
pub fn read(v: Vec<u32>) -> usize {
    v.clone().len()
}
pub fn owned(s: String) -> String {
    s.clone()
}
pub fn copied(x: u32) -> u32 {
    x.clone()
}
"#
        ),
    );
    assert_eq!(out.matches("borrows a copy of what it could borrow").count(), 2, "{out}");
    assert_eq!(out.matches("is only read by `.len(..)`").count(), 1, "{out}");
    assert!(out.contains("v.len()"), "the method's fix is offered:\n{out}");
    let after = fixed("clone_to_satisfy", FIXABLE);
    assert!(after.contains("takes(&s)\n"), "{after}");
    assert!(after.contains("takes(s)\n"), "{after}");
}

#[test]
fn unchecked_index_in_loop_finds_index_loops_and_fixes_the_plain_one() {
    const FIXABLE: &str = r#"
pub fn sum(v: &[u32]) -> u32 {
    let mut s = 0;
    for i in 0..v.len() {
        s += v[i];
    }
    s
}
"#;
    let out = found(
        "unchecked_index_in_loop",
        &format!(
            "{FIXABLE}{}",
            r#"
pub fn other(v: &[u32], w: &[u32]) -> u32 {
    let mut s = 0;
    for i in 0..v.len() {
        s += w[i];
    }
    s
}
pub fn next(v: &[u32]) -> u32 {
    let mut s = 0;
    for i in 0..v.len() {
        s += v[i + 1];
    }
    s
}
pub fn counted(v: &[u32]) -> usize {
    let mut s = 0;
    for i in 0..v.len() {
        if v[i] > 2 {
            s += i;
        }
    }
    s
}
pub fn set(v: &mut [u32]) {
    for i in 0..v.len() {
        v[i] = 0;
    }
}
"#
        ),
    );
    assert_eq!(out.matches("only to index it").count(), 2, "{out}");
    assert_eq!(out.matches("the loop counter of another sequence's length").count(), 1, "{out}");
    assert_eq!(out.matches("steps past the loop's range").count(), 1, "{out}");
    let after = fixed("unchecked_index_in_loop", FIXABLE);
    assert!(after.contains("for item in v.iter() {\n        s += *item;\n    }"), "{after}");
}

#[test]
fn default_numeric_fallback_len_finds_an_i32_cast_to_usize_and_offers_the_type() {
    let out = found(
        "default_numeric_fallback_len",
        r#"
pub fn counted(v: &[u32]) -> u32 {
    let mut n = 0;
    while n < 3 {
        n += 1;
    }
    v[n as usize]
}
pub fn typed(v: &[u32]) -> u32 {
    let n: usize = 0;
    v[n]
}
pub fn tied(v: &[u32]) -> u32 {
    let n = 0;
    v[n]
}
pub fn uncast() -> i32 {
    let n = 0;
    n + 1
}
"#,
    );
    assert_eq!(out.matches("falls back to `i32`").count(), 1, "{out}");
    assert!(out.contains("let mut n: usize = 0;"), "the fix is offered:\n{out}");
    let after = offered(
        "default_numeric_fallback_len",
        r#"
pub fn counted(v: &[u32]) -> u32 {
    let mut n = 0;
    while n < 3 {
        n += 1;
    }
    v[n as usize]
}
"#,
    );
    assert!(after.contains("    let mut n: usize = 0;\n"), "{after}");
    assert!(after.contains("    v[n]\n"), "the cast is dropped:\n{after}");
}

#[test]
fn refcell_borrow_across_call_finds_a_second_borrow_and_a_reentering_call() {
    let out = found(
        "refcell_borrow_across_call",
        r#"
use std::cell::RefCell;
pub struct S {
    cell: RefCell<u32>,
}
impl S {
    pub fn again(&self) {
        let _g = self.cell.borrow_mut();
        let _h = self.cell.borrow();
    }
    pub fn shared(&self) {
        let _g = self.cell.borrow();
        let _h = self.cell.borrow();
    }
    pub fn reenter(&self) {
        let _g = self.cell.borrow_mut();
        self.touch();
    }
    pub fn ended(&self) {
        let g = self.cell.borrow_mut();
        std::mem::drop(g);
        self.touch();
    }
    fn touch(&self) {}
}
"#,
    );
    assert_eq!(out.matches("guard of `self.cell` is held").count(), 2, "{out}");
    assert_eq!(out.matches("this borrows the cell again").count(), 1, "{out}");
    assert_eq!(out.matches("this call is handed the cell or what holds it").count(), 1, "{out}");
}

#[test]
fn unbounded_channel_finds_a_channel_with_no_bound_and_not_a_bounded_one() {
    let out = found(
        "unbounded_channel",
        r#"
pub fn open() {
    let _unbounded = crossbeam_channel::unbounded::<u32>();
    let _bounded = crossbeam_channel::bounded::<u32>(16);
}
"#,
    );
    assert_eq!(
        out.matches("use of a disallowed method `crossbeam_channel::unbounded`").count(),
        1,
        "{out}"
    );
    assert!(!out.contains("crossbeam_channel::bounded`"), "{out}");
}

#[test]
fn process_exit_finds_an_exit_outside_the_entry_module() {
    const CODE: &str = r#"
pub fn quit() -> ! {
    std::process::exit(2)
}
pub fn crash() -> ! {
    std::process::abort()
}
pub fn main() {
    std::process::exit(0)
}
mod helper {
    pub fn quit() -> ! {
        std::process::exit(3)
    }
}
"#;
    let lib = found("process_exit", CODE);
    assert_eq!(lib.matches("use of a disallowed method `std::process::exit`").count(), 3, "{lib}");
    assert_eq!(lib.matches("use of a disallowed method `std::process::abort`").count(), 1, "{lib}");
    let bin = found_in_bin("process_exit", CODE);
    assert_eq!(
        bin.matches("use of a disallowed method `std::process::exit`").count(),
        1,
        "only the helper module's, outside the entry module:\n{bin}"
    );
    assert!(!bin.contains("`std::process::abort`"), "{bin}");
}

#[test]
fn print_in_lib_finds_prints_in_library_code_only() {
    const CODE: &str = r#"
pub fn report(n: u32) {
    std::println!("{n}");
    std::eprintln!("{n}");
    let _shown = std::format!("{n}");
}
mod helper {
    pub fn report() {
        std::println!("x");
    }
}
"#;
    let lib = found("print_in_lib", CODE);
    assert_eq!(lib.matches("in library code writes to the process's").count(), 3, "{lib}");
    assert_eq!(lib.matches("`eprintln!` in library code writes to the process's stderr").count(), 1, "{lib}");
    let bin = found_in_bin("print_in_lib", CODE);
    assert_eq!(
        bin.matches("in library code writes to the process's").count(),
        1,
        "only the helper module's, outside the entry module:\n{bin}"
    );
    // With clippy's `print_stdout` on, the stdout places are its; stderr's stay.
    let shared = lint(&fixture_of(CODE), &format!("{} --warn=clippy::print_stdout", flags("print_in_lib")));
    assert_eq!(shared.matches("in library code writes to the process's").count(), 1, "{shared}");
}

#[test]
fn lock_unwrap_finds_a_poisoning_lock_unwrapped_unless_a_ban_reports_the_lock() {
    const CODE: &str = r#"
use std::sync::{Mutex, RwLock};
pub fn count(m: &Mutex<u32>) -> u32 {
    let _g = m.lock().unwrap();
    0
}
pub fn read(l: &RwLock<u32>) -> u32 {
    *l.read().expect("the lock")
}
pub fn write(l: &RwLock<u32>) {
    let _g = l.write().unwrap();
}
pub fn handled(m: &Mutex<u32>) -> bool {
    match m.lock() {
        Ok(_) => true,
        Err(_) => false,
    }
}
pub fn tried(m: &Mutex<u32>) {
    let _g = m.try_lock().unwrap();
}
"#;
    let out = found("lock_unwrap", CODE);
    assert_eq!(out.matches("panics once any thread panicked holding this lock").count(), 3, "{out}");
    assert!(out.contains("`Mutex::lock(..).unwrap(..)`"), "{out}");
    assert!(out.contains("`RwLock::read(..).expect(..)`"), "{out}");
    let conf = Conf {
        table: r#"disallowed-types = [{ path = "std::sync::Mutex" }]"#.parse().expect("the configuration reads"),
        ..Conf::default()
    };
    let flags = format!("{} --warn=clippy::disallowed_types", flags("lock_unwrap"));
    let banned = crate::tests::lint_with(&fixture_of(CODE), &flags, conf);
    assert_eq!(
        banned.matches("panics once any thread panicked holding this lock").count(),
        2,
        "the `Mutex` is the ban's, the `RwLock`'s are this lint's:\n{banned}"
    );
}

#[test]
fn dropped_error_finds_a_flatten_of_results_and_not_of_options() {
    let out = found(
        "dropped_error",
        r#"
pub struct ReadError;
pub struct Reads;
impl Iterator for Reads {
    type Item = Result<u32, ReadError>;
    fn next(&mut self) -> Option<Result<u32, ReadError>> {
        None
    }
}
pub struct Maybes;
impl Iterator for Maybes {
    type Item = Option<u32>;
    fn next(&mut self) -> Option<Option<u32>> {
        None
    }
}
pub fn firsts() -> Option<u32> {
    let _maybe = Maybes.flatten().next();
    Reads.flatten().next()
}
"#,
    );
    assert_eq!(out.matches("`.flatten()` throws away the `ReadError` of each `Err`").count(), 1, "{out}");
}
