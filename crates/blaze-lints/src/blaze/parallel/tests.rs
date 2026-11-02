//! `parallelizable` and `serial_dependence` over fixtures with an `std` of their own, whose
//! iterators, slices and collections have bodies the way the real ones do: a slice's `Iter`
//! reads through the slice it holds, `IterMut` hands out pointers into it, `Range` counts, the
//! adaptors call their closures, `io::print` calls a foreign `write`.
//!
//! A line of the crate `main` says what it expects: `//~ parallelizable` that the loop or chain
//! starting on it is found parallel, `//~ serial_dependence` that it is found serial for one
//! dependence. Every other loop must be found neither: the near misses.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{conf::Conf, tests::lint_with};

pub(super) const MINICORE: &str = "//- minicore: sized, copy, clone, fn, option, range, send, sync, index, slice, phantom_data, drop, cell, builtin_impls\n";

pub(super) const STD: &str = r#"
//- /std/lib.rs crate:std
#![feature(lang_items, negative_impls)]
pub mod prelude {
    pub mod v1 {
        pub use crate::iter::{IntoIterator, Iterator};
        pub use crate::slice::SliceExt;
        pub use crate::vec::Vec;
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod marker {
    pub use core::marker::{PhantomData, Send, Sized, Sync};
}
pub mod intrinsics {
    #[rustc_intrinsic]
    pub unsafe fn offset<Ptr, Delta>(dst: Ptr, offset: Delta) -> Ptr;
}
pub mod iter {
    #[lang = "iterator"]
    pub trait Iterator {
        type Item;
        #[lang = "next"]
        fn next(&mut self) -> Option<Self::Item>;
        fn map<B, F: FnMut(Self::Item) -> B>(self, f: F) -> Map<Self, F>
        where
            Self: Sized,
        {
            Map { iter: self, f }
        }
        fn filter_map<B, F: FnMut(Self::Item) -> Option<B>>(self, f: F) -> FilterMap<Self, F>
        where
            Self: Sized,
        {
            FilterMap { iter: self, f }
        }
        fn enumerate(self) -> Enumerate<Self>
        where
            Self: Sized,
        {
            Enumerate { iter: self, count: 0 }
        }
        fn sum<S: Sum<Self::Item>>(self) -> S
        where
            Self: Sized,
        {
            S::sum(self)
        }
        fn count(self) -> usize
        where
            Self: Sized,
        {
            let mut n = 0;
            let mut iter = self;
            while let Some(_) = iter.next() {
                n += 1;
            }
            n
        }
        fn for_each<F: FnMut(Self::Item)>(self, mut f: F)
        where
            Self: Sized,
        {
            let mut iter = self;
            while let Some(item) = iter.next() {
                f(item);
            }
        }
    }
    pub trait Sum<A = Self> {
        fn sum<I: Iterator<Item = A>>(iter: I) -> Self;
    }
    impl Sum for u64 {
        fn sum<I: Iterator<Item = u64>>(mut iter: I) -> u64 {
            let mut sum = 0;
            while let Some(item) = iter.next() {
                sum += item;
            }
            sum
        }
    }
    impl Sum for f64 {
        fn sum<I: Iterator<Item = f64>>(mut iter: I) -> f64 {
            let mut sum = 0.0;
            while let Some(item) = iter.next() {
                sum += item;
            }
            sum
        }
    }
    pub trait IntoIterator {
        type Item;
        type IntoIter: Iterator<Item = Self::Item>;
        #[lang = "into_iter"]
        fn into_iter(self) -> Self::IntoIter;
    }
    impl<I: Iterator> IntoIterator for I {
        type Item = I::Item;
        type IntoIter = I;
        fn into_iter(self) -> I {
            self
        }
    }
    pub struct Map<I, F> {
        iter: I,
        f: F,
    }
    impl<B, I: Iterator, F: FnMut(I::Item) -> B> Iterator for Map<I, F> {
        type Item = B;
        fn next(&mut self) -> Option<B> {
            match self.iter.next() {
                Some(item) => Some((self.f)(item)),
                None => None,
            }
        }
    }
    pub struct FilterMap<I, F> {
        iter: I,
        f: F,
    }
    impl<B, I: Iterator, F: FnMut(I::Item) -> Option<B>> Iterator for FilterMap<I, F> {
        type Item = B;
        fn next(&mut self) -> Option<B> {
            loop {
                match self.iter.next() {
                    Some(item) => {
                        if let Some(mapped) = (self.f)(item) {
                            return Some(mapped);
                        }
                    }
                    None => return None,
                }
            }
        }
    }
    pub struct Enumerate<I> {
        iter: I,
        count: usize,
    }
    impl<I: Iterator> Iterator for Enumerate<I> {
        type Item = (usize, I::Item);
        fn next(&mut self) -> Option<(usize, I::Item)> {
            match self.iter.next() {
                Some(item) => {
                    let count = self.count;
                    self.count = count + 1;
                    Some((count, item))
                }
                None => None,
            }
        }
    }
    impl Iterator for core::ops::Range<usize> {
        type Item = usize;
        fn next(&mut self) -> Option<usize> {
            if self.start < self.end {
                let n = self.start;
                self.start = n + 1;
                Some(n)
            } else {
                None
            }
        }
    }
}
pub mod slice {
    use core::marker::PhantomData;
    use crate::iter::Iterator;
    pub struct Iter<'a, T> {
        slice: &'a [T],
        at: usize,
    }
    impl<'a, T> Iterator for Iter<'a, T> {
        type Item = &'a T;
        fn next(&mut self) -> Option<&'a T> {
            if self.at < self.slice.len() {
                let item = &self.slice[self.at];
                self.at += 1;
                Some(item)
            } else {
                None
            }
        }
    }
    pub struct IterMut<'a, T> {
        ptr: *mut T,
        len: usize,
        at: usize,
        _marker: PhantomData<&'a mut T>,
    }
    unsafe impl<T: Send> Send for IterMut<'_, T> {}
    unsafe impl<T: Sync> Sync for IterMut<'_, T> {}
    impl<'a, T> Iterator for IterMut<'a, T> {
        type Item = &'a mut T;
        fn next(&mut self) -> Option<&'a mut T> {
            if self.at < self.len {
                let item = unsafe { &mut *crate::intrinsics::offset(self.ptr, self.at) };
                self.at += 1;
                Some(item)
            } else {
                None
            }
        }
    }
    pub trait SliceExt<T> {
        fn iter(&self) -> Iter<'_, T>;
        fn iter_mut(&mut self) -> IterMut<'_, T>;
    }
    impl<T> SliceExt<T> for [T] {
        fn iter(&self) -> Iter<'_, T> {
            Iter { slice: self, at: 0 }
        }
        fn iter_mut(&mut self) -> IterMut<'_, T> {
            let len = self.len();
            IterMut { ptr: self as *mut [T] as *mut T, len, at: 0, _marker: PhantomData }
        }
    }
}
pub mod cmp {
    pub fn min(a: u64, b: u64) -> u64 {
        if a < b { a } else { b }
    }
    pub fn max(a: u64, b: u64) -> u64 {
        if a < b { b } else { a }
    }
}
pub mod cell {
    pub use core::cell::{Cell, UnsafeCell};
    impl<T: ?Sized> !Sync for UnsafeCell<T> {}
    pub struct RefCell<T> {
        value: UnsafeCell<T>,
    }
    impl<T> RefCell<T> {
        pub fn new(value: T) -> RefCell<T> {
            RefCell { value: UnsafeCell::new(value) }
        }
        pub fn borrow_mut(&self) -> &mut T {
            unsafe { &mut *self.value.get() }
        }
    }
}
pub mod vec {
    pub struct Vec<T> {
        len: usize,
        last: Option<T>,
    }
    impl<T> Vec<T> {
        pub const fn new() -> Vec<T> {
            Vec { len: 0, last: None }
        }
        pub fn push(&mut self, value: T) {
            self.len += 1;
            self.last = Some(value);
        }
    }
}
pub mod collections {
    pub struct BTreeSet<T> {
        len: usize,
        last: Option<T>,
    }
    impl<T> BTreeSet<T> {
        pub const fn new() -> BTreeSet<T> {
            BTreeSet { len: 0, last: None }
        }
        pub fn insert(&mut self, value: T) -> bool {
            self.len += 1;
            self.last = Some(value);
            true
        }
        pub fn len(&self) -> usize {
            self.len
        }
    }
    pub struct HashSet<T> {
        len: usize,
        last: Option<T>,
    }
    impl<T> HashSet<T> {
        pub const fn new() -> HashSet<T> {
            HashSet { len: 0, last: None }
        }
        pub fn insert(&mut self, value: T) -> bool {
            self.len += 1;
            self.last = Some(value);
            true
        }
    }
}
pub mod io {
    extern "C" {
        fn write(fd: i32, buf: *const u8, len: usize) -> isize;
    }
    pub fn print(value: u64) {
        unsafe {
            write(1, &value as *const u64 as *const u8, 8);
        }
    }
}
"#;

pub(super) const MAIN: &str = "//- /src/lib.rs crate:main deps:std\n";

pub(super) const FLAGS: &str = "-A clippy::all --warn=blaze::parallelizable --warn=blaze::serial_dependence";

/// The lint a message is of.
fn lint_of(message: &str) -> Option<&'static str> {
    if message.starts_with("this loop's iterations are independent")
        || message.starts_with("this iterator chain's items are independent")
    {
        Some("parallelizable")
    } else if message.contains("but for one dependence") {
        Some("serial_dependence")
    } else {
        None
    }
}

/// The findings in `src/lib.rs` of `out`, as (line, lint).
pub(super) fn found(out: &str) -> Vec<(u32, &'static str)> {
    let mut found = Vec::new();
    let mut lint = None;
    for line in out.lines() {
        if let Some(message) = line.strip_prefix("warning: ") {
            lint = lint_of(message);
            continue;
        }
        let Some(at) = line.trim_start().strip_prefix("--> src/lib.rs:") else { continue };
        let Some(name) = lint.take() else { continue };
        let number = at.split(':').next().and_then(|it| it.parse().ok());
        found.push((number.expect("a line number after the path"), name));
    }
    found.sort_unstable();
    found
}

/// What the markers of `main` expect, as (line, lint).
fn expected(main: &str) -> Vec<(u32, &'static str)> {
    let mut expected = Vec::new();
    for (index, line) in main.lines().enumerate() {
        let Some((_, marks)) = line.split_once("//~") else { continue };
        for mark in marks.split_whitespace() {
            let lint = ["parallelizable", "serial_dependence"]
                .into_iter()
                .find(|&it| it == mark)
                .unwrap_or_else(|| panic!("`{mark}` is not a lint of this module's"));
            expected.push((index as u32 + 1, lint));
        }
    }
    expected.sort_unstable();
    expected
}

/// A configuration read from `text`, as `clippy.toml` would have it.
pub(super) fn conf(text: &str) -> Conf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "blaze-lints-parallel-{}-{}.toml",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, text).unwrap();
    let conf = Conf::load(Some(path.clone()), None).unwrap();
    std::fs::remove_file(&path).unwrap();
    conf
}

/// A threshold low enough that a loop of a few statements over the assumed thousand items
/// passes it, and one over four does not.
const LOW: &str = "parallel-threshold = 1000\n";

/// Lints `main` with `conf`, checks the findings are what its markers say, and gives the
/// output.
fn check_with(main: &str, conf: Conf) -> String {
    let main = main.trim_start_matches('\n');
    let fixture = format!("{MINICORE}{STD}{MAIN}{main}");
    let out = lint_with(&fixture, FLAGS, conf);
    assert_eq!(found(&out), expected(main), "the findings of:\n{main}\nwere:\n{out}");
    out
}

fn check(main: &str) -> String {
    check_with(main, conf(LOW))
}

#[test]
fn map_filter_map_and_for_each_chains() {
    let out = check(
        r#"
pub fn doubled_sum(xs: &[u64]) -> u64 {
    xs.iter().map(|x| *x * 2).sum() //~ parallelizable
}
pub fn big(xs: &[u64]) -> usize {
    xs.iter().filter_map(|x| if *x > 3 { Some(*x) } else { None }).count() //~ parallelizable
}
pub fn scale(xs: &mut [u64]) {
    xs.iter_mut().for_each(|x| *x = *x * 3 + 1); //~ parallelizable
}
"#,
    );
    assert!(out.contains("par_iter"), "{out}");
    assert!(out.contains("par_iter_mut"), "{out}");
}

#[test]
fn disjoint_writes_at_the_own_index_and_through_the_own_item() {
    let out = check(
        r#"
pub fn tripled(out: &mut [u64], xs: &[u64]) {
    for i in 0..100_000 { //~ parallelizable
        out[i] = xs[i] * 3;
    }
}
pub fn bumped(xs: &mut [u64]) {
    for x in xs.iter_mut() { //~ parallelizable
        *x = *x * 3 + 1;
    }
}
"#,
    );
    assert!(out.contains("each iteration writes `out` only at its own index"), "{out}");
    assert!(out.contains("each iteration writes only through its own item"), "{out}");
    assert!(out.contains("(0..100_000).into_par_iter()"), "{out}");
    assert!(out.contains("xs.par_iter_mut().for_each(|x| {"), "{out}");
}

#[test]
fn each_reduction() {
    let out = check(
        r#"
use std::collections::BTreeSet;
pub fn sum(xs: &[u64]) -> u64 {
    let mut total = 0;
    for x in xs.iter() { //~ parallelizable
        total += *x;
    }
    total
}
pub fn product(xs: &[u64]) -> u64 {
    let mut product = 1;
    for x in xs.iter() { //~ parallelizable
        product *= *x;
    }
    product
}
pub fn count(xs: &[u64]) -> usize {
    let mut n = 0;
    for x in xs.iter() { //~ parallelizable
        if *x > 3 {
            n += 1;
        }
    }
    n
}
pub fn or(xs: &[u64]) -> u64 {
    let mut bits = 0;
    for x in xs.iter() { //~ parallelizable
        bits |= *x;
    }
    bits
}
pub fn and(xs: &[u64]) -> u64 {
    let mut bits = !0;
    for x in xs.iter() { //~ parallelizable
        bits &= *x;
    }
    bits
}
pub fn xor(xs: &[u64]) -> u64 {
    let mut bits = 0;
    for x in xs.iter() { //~ parallelizable
        bits ^= *x;
    }
    bits
}
pub fn least(xs: &[u64]) -> u64 {
    let mut least = 1 << 63;
    for x in xs.iter() { //~ parallelizable
        least = std::cmp::min(least, *x);
    }
    least
}
pub fn most(xs: &[u64]) -> u64 {
    let mut most = 0;
    for x in xs.iter() { //~ parallelizable
        most = std::cmp::max(most, *x);
    }
    most
}
pub fn distinct(xs: &[u64]) -> usize {
    let mut seen = BTreeSet::new();
    for x in xs.iter() { //~ parallelizable
        seen.insert(*x);
    }
    seen.len()
}
"#,
    );
    for said in [
        "`total` is a sum (`+`)",
        "`product` is a product (`*`)",
        "`n` is a count",
        "`bits` is an `|`",
        "`bits` is an `&`",
        "`bits` is an `^`",
        "`least` is a minimum",
        "`most` is a maximum",
        "`seen` is a set every iteration inserts into",
    ] {
        assert!(out.contains(said), "no `{said}` in:\n{out}");
    }
    assert!(out.contains(".map(|..| ..).sum() for `total`"), "{out}");
}

#[test]
fn a_float_sum_only_when_reassociated() {
    let out = check(
        r#"
pub fn sum(xs: &[f64]) -> f64 {
    let mut total = 0.0;
    for x in xs.iter() { //~ serial_dependence
        total += *x;
    }
    total
}
pub fn chained(xs: &[f64]) -> f64 {
    xs.iter().map(|x| *x * 2.0).sum() //~ serial_dependence
}
#[blaze::reassociate]
pub fn reassociated(xs: &[f64]) -> f64 {
    let mut total = 0.0;
    for x in xs.iter() { //~ parallelizable
        total += *x;
    }
    total
}
"#,
    );
    assert!(out.contains("of floats, which rounds differently in another order"), "{out}");
    assert!(out.contains("only because the function is `#[blaze::reassociate]`"), "{out}");
}

#[test]
fn near_misses() {
    let out = check(
        r#"
use std::cell::RefCell;
use std::collections::HashSet;
pub fn hashed(xs: &[u64]) -> u64 {
    let mut hash = 0u64;
    for x in xs.iter() { //~ serial_dependence
        hash = hash * 31 + *x;
    }
    hash
}
pub fn through_a_parameter(total: &mut u64, xs: &[u64]) {
    for x in xs.iter() {
        *total += *x;
    }
}
pub fn shifted(out: &mut [u64]) {
    for i in 0..100_000 { //~ serial_dependence
        out[i] = out[i + 1];
    }
}
pub fn in_a_ref_cell(cell: &RefCell<u64>, xs: &[u64]) {
    for x in xs.iter() {
        *cell.borrow_mut() += *x;
    }
}
pub fn printed(xs: &[u64]) {
    for x in xs.iter() { //~ serial_dependence
        std::io::print(*x * 2);
    }
}
pub fn unknown(f: &dyn Fn(u64) -> u64, out: &mut [u64]) {
    for i in 0..100_000 {
        out[i] = f(i as u64);
    }
}
pub fn pushed(xs: &[u64]) -> Vec<u64> {
    let mut out = Vec::new();
    for x in xs.iter() { //~ serial_dependence
        out.push(*x * 2);
    }
    out
}
pub fn hash_set(xs: &[u64]) -> HashSet<u64> {
    let mut seen = HashSet::new();
    for x in xs.iter() { //~ serial_dependence
        seen.insert(*x);
    }
    seen
}
pub fn tiny(out: &mut [u64], xs: &[u64]) {
    for i in 0..4 {
        out[i] = xs[i] * 3;
    }
}
"#,
    );
    assert!(out.contains("`hash` is carried from one iteration to the next"), "{out}");
    assert!(out.contains("an iteration reads `out` at an index another iteration writes"), "{out}");
    assert!(out.contains("calls `write`, a foreign function (I/O, the OS)"), "{out}");
    assert!(out.contains("`out` is pushed onto in the order the iterations run"), "{out}");
    assert!(out.contains("`seen` is a `HashSet`"), "{out}");
}

#[test]
fn the_default_threshold_passes_only_big_loops() {
    let main = r#"
pub fn tiny(out: &mut [u64], xs: &[u64]) {
    for i in 0..4 {
        out[i] = xs[i] * 3;
    }
}
pub fn three() -> u64 {
    let mut total = 0;
    for x in [1u64, 2, 3].iter() {
        total += *x;
    }
    total
}
pub fn big(out: &mut [u64], xs: &[u64]) {
    for i in 0..1_000_000 { //~ parallelizable
        out[i] = xs[i] * 3;
    }
}
"#;
    let out = check_with(main, Conf::default());
    assert!(out.contains("past `parallel-threshold` (50000)"), "{out}");
}

#[test]
fn the_threshold_and_the_assumed_trips_come_from_the_configuration() {
    let main = r#"
pub fn few(xs: &[u64]) -> u64 {
    let mut total = 0;
    for x in xs.iter() { //~ parallelizable
        total += *x;
    }
    total
}
"#;
    let out = check_with(main, conf("parallel-threshold = 100\nparallel-assumed-trips = 64\n"));
    assert!(out.contains("over 64 iterations (not known; `parallel-assumed-trips`)"), "{out}");
    assert!(out.contains("past `parallel-threshold` (100)"), "{out}");
}

#[test]
fn off_unless_asked_for() {
    let main = r#"
pub fn big(out: &mut [u64], xs: &[u64]) {
    for i in 0..1_000_000 {
        out[i] = xs[i] * 3;
    }
}
"#;
    let fixture = format!("{MINICORE}{STD}{MAIN}{main}");
    let out = lint_with(&fixture, "-A clippy::all", Conf::default());
    assert_eq!(found(&out), [], "{out}");
}
