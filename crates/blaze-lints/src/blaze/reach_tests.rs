//! The proved-unreachable checker (docs/v2/plan/design/proved-unreachable.md): per pattern the
//! prover discharges, a test that it passes and a near miss that is still reported.

use crate::tests::lint;

const MINICORE: &str = "//- minicore: sized, copy, clone, drop, option, result, fn, panic, index, slice, builtin_impls, allocator_api, range, iterator, non_zero, reach, blaze_unreachable, todo\n";

/// `alloc` with a `Vec` whose index checks its length and panics, as the real one does.
const TOOLCHAIN: &str = r#"
//- /alloc.rs crate:alloc
pub mod vec {
    pub struct Vec<T> {
        ptr: *mut T,
        len: usize,
    }
    impl<T> Vec<T> {
        pub fn len(&self) -> usize {
            self.len
        }
        pub fn is_empty(&self) -> bool {
            self.len == 0
        }
        pub fn push(&mut self, _value: T) {
            self.len = self.len + 1;
        }
        pub fn clear(&mut self) {
            self.len = 0;
        }
    }
    impl<T> core::ops::Index<usize> for Vec<T> {
        type Output = T;
        fn index(&self, index: usize) -> &T {
            if index >= self.len {
                core::panicking::panic("index out of bounds");
            }
            unsafe { &*self.ptr }
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
pub mod vec { pub use alloc::vec::Vec; }
"#;

fn linted(main: &str, flags: &str) -> String {
    let fixture = format!(
        "{MINICORE}{TOOLCHAIN}//- /src/lib.rs crate:main deps:alloc,std\n{}",
        main.trim_start_matches('\n')
    );
    lint(&fixture, flags)
}

/// The names of the `#[blaze::nopanic]` functions of `main` the lint refuses, in order.
fn refused(main: &str) -> Vec<String> {
    let out = linted(main, "-A clippy::all --deny=blaze::nopanic");
    let mut names: Vec<String> = out
        .lines()
        .filter(|line| line.contains("is `#[blaze::nopanic]`"))
        .filter_map(|line| line.split('`').nth(1))
        .map(|name| name.rsplit("::").next().unwrap_or(name).to_owned())
        .collect();
    names.dedup();
    names
}

/// `v[i]` under `if i < v.len()` cannot fail. Near misses: the check is of another index;
/// the index is reassigned between the check and the use; the check is on one path only.
#[test]
fn a_slice_index_under_its_bound_check_is_no_panic() {
    let names = refused(
        r#"
#[blaze::nopanic]
pub fn guarded(v: &[u32], i: usize) -> u32 {
    if i < v.len() { v[i] } else { 0 }
}
#[blaze::nopanic]
pub fn early_return(v: &[u32], i: usize) -> u32 {
    if i >= v.len() {
        return 0;
    }
    v[i]
}
#[blaze::nopanic]
pub fn both(v: &[u32], i: usize, j: usize) -> u32 {
    if i < v.len() && j < v.len() { v[i] ^ v[j] } else { 0 }
}
#[blaze::nopanic]
pub fn other_index(v: &[u32], i: usize, j: usize) -> u32 {
    if i < v.len() { v[j] } else { 0 }
}
#[blaze::nopanic]
pub fn reassigned(v: &[u32], mut i: usize, j: usize) -> u32 {
    if i < v.len() {
        i = j;
        v[i]
    } else {
        0
    }
}
#[blaze::nopanic]
pub fn not_dominating(v: &[u32], i: usize, flag: bool) -> u32 {
    if flag {
        if i < v.len() {
            return 1;
        }
    }
    v[i]
}
#[blaze::nopanic]
pub fn other_slice(v: &[u32], w: &[u32], i: usize) -> u32 {
    if i < w.len() { v[i] } else { 0 }
}
"#,
    );
    assert_eq!(names, ["other_index", "reassigned", "not_dominating", "other_slice"]);
}

/// `o.unwrap()` under `if o.is_some()`, under `if let Some(_) = o` and in a `match` arm that
/// knows the variant cannot fail. Near misses: the check is of another option; the option is
/// reassigned between; the check is `is_none`; the value went through a function no model
/// names (`Result::ok`).
#[test]
fn an_unwrap_of_a_known_variant_is_no_panic() {
    let names = refused(
        r#"
#[blaze::nopanic]
pub fn is_some(o: Option<u32>) -> u32 {
    if o.is_some() { o.unwrap() } else { 0 }
}
#[blaze::nopanic]
pub fn not_none(o: Option<u32>) -> u32 {
    if o.is_none() {
        return 0;
    }
    o.unwrap()
}
#[blaze::nopanic]
pub fn if_let(o: Option<u32>) -> u32 {
    if let Some(_) = o { o.unwrap() } else { 0 }
}
#[blaze::nopanic]
pub fn in_match(o: Option<u32>) -> u32 {
    match o {
        Some(_) => o.unwrap(),
        None => 0,
    }
}
#[blaze::nopanic]
pub fn by_ref(o: &Option<u32>) -> u32 {
    if o.is_some() { o.unwrap() } else { 0 }
}
#[blaze::nopanic]
pub fn is_ok(r: Result<u32, u8>) -> u32 {
    if r.is_ok() { r.ok().unwrap() } else { 0 }
}
#[blaze::nopanic]
pub fn other_option(o: Option<u32>, p: Option<u32>) -> u32 {
    if o.is_some() { p.unwrap() } else { 0 }
}
#[blaze::nopanic]
pub fn reassigned(mut o: Option<u32>, p: Option<u32>) -> u32 {
    if o.is_some() {
        o = p;
        o.unwrap()
    } else {
        0
    }
}
#[blaze::nopanic]
pub fn wrong_way(o: Option<u32>) -> u32 {
    if o.is_none() { o.unwrap() } else { 0 }
}
#[blaze::nopanic]
pub fn unchecked(o: Option<u32>) -> u32 {
    o.unwrap()
}
"#,
    );
    assert_eq!(names, ["is_ok", "other_option", "reassigned", "wrong_way", "unchecked"]);
}

/// `a / b` and `a % b` under `if b != 0`, and by a `NonZero`'s value, cannot fail. Near
/// misses: the check is of the dividend; the divisor is reassigned between; no check.
#[test]
fn a_division_by_a_checked_divisor_is_no_panic() {
    let names = refused(
        r#"
use core::num::NonZero;
#[blaze::nopanic]
pub fn checked(a: u32, b: u32) -> u32 {
    if b != 0 { a / b } else { 0 }
}
#[blaze::nopanic]
pub fn early(a: u32, b: u32) -> u32 {
    if b == 0 {
        return 0;
    }
    a % b
}
#[blaze::nopanic]
pub fn positive(a: u32, b: u32) -> u32 {
    if b > 0 { a / b } else { 0 }
}
#[blaze::nopanic]
pub fn nonzero(a: u32, b: NonZero<u32>) -> u32 {
    a / b.get()
}
#[blaze::nopanic]
pub fn dividend(a: u32, b: u32) -> u32 {
    if a != 0 { a / b } else { 0 }
}
#[blaze::nopanic]
pub fn reassigned(a: u32, mut b: u32, c: u32) -> u32 {
    if b != 0 {
        b = c;
        a / b
    } else {
        0
    }
}
#[blaze::nopanic]
pub fn unchecked(a: u32, b: u32) -> u32 {
    a / b
}
"#,
    );
    assert_eq!(names, ["dividend", "reassigned", "unchecked"]);
}

/// A `Vec`'s index, whose panic is in the callee: `v[i]` under `if i < v.len()`, `v[0]` under
/// `if !v.is_empty()`, and `v[i]` in `for i in 0..v.len()`. Near misses: the vector is pushed
/// to or cleared between the check and the use (through `&mut`); another vector's length;
/// a loop over another bound.
#[test]
fn a_vec_index_under_its_length_check_is_no_panic() {
    let names = refused(
        r#"
use std::vec::Vec;
#[blaze::nopanic]
pub fn guarded(v: &Vec<u32>, i: usize) -> u32 {
    if i < v.len() { v[i] } else { 0 }
}
#[blaze::nopanic]
pub fn not_empty(v: &Vec<u32>) -> u32 {
    if !v.is_empty() { v[0] } else { 0 }
}
#[blaze::nopanic]
pub fn each(v: &Vec<u32>) -> u32 {
    let mut sum = 0;
    for i in 0..v.len() {
        sum ^= v[i];
    }
    sum
}
#[blaze::nopanic]
pub fn counted(v: &[u32]) -> u32 {
    let mut sum = 0;
    let mut i = 0;
    while i < v.len() {
        sum ^= v[i];
        i += 1;
    }
    sum
}
#[blaze::nopanic]
pub fn owned(v: Vec<u32>, i: usize) -> u32 {
    if i < v.len() { v[i] } else { 0 }
}
#[blaze::nopanic]
pub fn cleared(v: &mut Vec<u32>, i: usize) -> u32 {
    if i < v.len() {
        v.clear();
        v[i]
    } else {
        0
    }
}
#[blaze::nopanic]
pub fn other_vec(v: &Vec<u32>, w: &Vec<u32>, i: usize) -> u32 {
    if i < w.len() { v[i] } else { 0 }
}
#[blaze::nopanic]
pub fn other_bound(v: &Vec<u32>, n: usize) -> u32 {
    let mut sum = 0;
    for i in 0..n {
        sum ^= v[i];
    }
    sum
}
#[blaze::nopanic]
pub fn empty_unchecked(v: &Vec<u32>) -> u32 {
    v[0]
}
"#,
    );
    assert_eq!(names, ["cleared", "other_vec", "other_bound", "empty_unchecked"]);
}

/// `v.first().unwrap()` under `if !v.is_empty()`. Near miss: no check.
#[test]
fn first_of_a_slice_known_not_empty_is_some() {
    let names = refused(
        r#"
#[blaze::nopanic]
pub fn first(v: &[u32]) -> u32 {
    if !v.is_empty() { *v.first().unwrap() } else { 0 }
}
#[blaze::nopanic]
pub fn by_len(v: &[u32]) -> u32 {
    if v.len() > 0 { v[0] } else { 0 }
}
#[blaze::nopanic]
pub fn unchecked(v: &[u32]) -> u32 {
    *v.first().unwrap()
}
"#,
    );
    assert_eq!(names, ["unchecked"]);
}
