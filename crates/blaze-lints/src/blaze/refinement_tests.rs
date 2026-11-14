//! `blaze::refinement` (docs/v2/plan/design/refined-values.md): per check, a site it proves and
//! a near miss it refuses, with what the refusal names.

use crate::tests::lint;

const MINICORE: &str = "//- minicore: sized, copy, clone, drop, option, result, fn, panic, index, slice, builtin_impls, allocator_api, range, iterator, non_zero, reach, blaze_unreachable\n";

/// `alloc` with a `Vec` whose methods the prover's table names (`push`, `pop`, `new`,
/// `vec![x; n]`'s `from_elem`), as the real one has them.
const TOOLCHAIN: &str = r#"
//- /alloc.rs crate:alloc
pub mod vec {
    pub struct Vec<T> {
        ptr: *mut T,
        len: usize,
    }
    impl<T> Vec<T> {
        pub fn new() -> Vec<T> {
            Vec { ptr: 0 as *mut T, len: 0 }
        }
        pub fn len(&self) -> usize {
            self.len
        }
        pub fn push(&mut self, _value: T) {
            self.len = self.len + 1;
        }
        pub fn pop(&mut self) -> Option<T> {
            None
        }
    }
    pub fn from_elem<T>(_elem: T, n: usize) -> Vec<T> {
        Vec { ptr: 0 as *mut T, len: n }
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

/// The findings of `blaze::refinement` alone.
fn refinement(main: &str) -> String {
    linted(main, "-A clippy::all -A blaze::unreachable")
}

fn count(out: &str) -> usize {
    out.matches("blaze::refinement").count()
}

/// `Twice` three ways: from a `u30`, which holds exactly the range; from an `i32` under a
/// dominating check; from an `i32` with no check, refused with the range it needs. An
/// out-of-range constant is refused the same way.
#[test]
fn twice_is_built_only_where_its_range_is_shown() {
    let out = refinement(
        r#"
#![blaze::arbitrary_ints]
pub mod twice {
    #[blaze::invariant(0 <= self.x && self.x <= i32::MAX / 2)]
    pub struct Twice {
        x: i32,
    }
    impl Twice {
        pub fn narrow(x: u30) -> Twice {
            Twice { x: x as i32 }
        }
        pub fn checked(x: i32) -> Option<Twice> {
            if 0 <= x && x <= 1073741823 { Some(Twice { x }) } else { None }
        }
        pub fn new(x: i32) -> Twice {
            Twice { x }
        }
        pub fn too_big() -> Twice {
            Twice { x: 2000000000 }
        }
        pub fn doubled(&self) -> i32 {
            self.x * 2
        }
    }
}
"#,
    );
    assert!(out.contains("`x` must be in `0..=1073741823`"), "{out}");
    assert!(out.contains("`u30`"), "the note names the narrower type: {out}");
    assert!(out.contains("2000000000"), "the constant is refused: {out}");
    // `narrow`, `checked` and `doubled` are not.
    assert_eq!(out.matches("this `Twice { .. }`").count(), 2, "{out}");
}

/// An invariant blaze can't check is refused at the type, as is one whose field another
/// module could set; a program with neither invariants nor contracts has no finding.
#[test]
fn an_invariant_outside_the_fragment_or_on_a_public_field_is_refused() {
    let out = refinement(
        r#"
pub mod a {
    #[blaze::invariant(self.x * self.y < 10)]
    pub struct Product {
        x: i32,
        y: i32,
    }
    #[blaze::invariant(self.x >= 0)]
    pub struct Open {
        pub x: i32,
    }
}
"#,
    );
    assert!(out.contains("not checkable: `#[blaze::invariant(self.x * self.y < 10)]`"), "{out}");
    assert!(out.contains("narrower type or a simpler bound"), "{out}");
    assert!(out.contains("`Open.x` is not private"), "{out}");
    let plain = refinement(
        r#"
pub fn sum(v: &[u32]) -> u32 {
    let mut total = 0;
    for i in 0..v.len() {
        total = total ^ v[i];
    }
    total
}
"#,
    );
    assert_eq!(count(&plain), 0, "{plain}");
}

/// A `requires` shown by an `if`, by a constant and by a loop's own bound; one not shown at
/// all is refused, naming the relation that is missing.
#[test]
fn a_requires_is_shown_at_each_call() {
    let out = refinement(
        r#"
#![feature(blaze_verify)]
use alloc::vec::Vec;
pub fn at(v: &Vec<u32>, i: usize) -> u32 requires i < v.len() {
    v[i]
}
pub fn half(x: u32) -> u32 requires x < 100 {
    x / 2
}
pub fn under_check(v: &Vec<u32>, i: usize) -> u32 {
    if i < v.len() { at(v, i) } else { 0 }
}
pub fn by_constant() -> u32 {
    half(7)
}
pub fn loop_index(v: &Vec<u32>) -> u32 {
    let mut total = 0;
    for i in 0..v.len() {
        total = total ^ at(v, i);
    }
    total
}
pub fn unbounded(v: &Vec<u32>, i: usize) -> u32 {
    at(v, i)
}
pub fn too_big() -> u32 {
    half(100)
}
"#,
    );
    assert!(out.contains("this call of `at` needs `requires i < v.len()`"), "{out}");
    assert!(out.contains("this call of `half` needs `requires x < 100`"), "{out}");
    assert_eq!(out.matches("this call of").count(), 2, "{out}");
}

/// A constructor's `ensures` on a length is what its callers index by; a method that pushes
/// to one of two vectors whose lengths its type's invariant ties is refused at its end.
#[test]
fn measures_relate_lengths_across_calls_and_fields() {
    let out = refinement(
        r#"
#![feature(blaze_verify)]
use alloc::vec::Vec;
pub fn at(v: &Vec<u32>, i: usize) -> u32 requires i < v.len() {
    v[i]
}
pub fn zeros(n: usize) -> (r: Vec<u32>) ensures r.len() == n {
    alloc::vec::from_elem(0, n)
}
pub fn last_zero(n: usize) -> u32 {
    let v = zeros(n);
    if n > 0 { at(&v, n - 1) } else { 0 }
}
pub mod spans {
    use alloc::vec::Vec;
    #[blaze::invariant(self.starts.len() == self.ends.len())]
    pub struct Spans {
        starts: Vec<u32>,
        ends: Vec<u32>,
    }
    impl Spans {
        pub fn new() -> Spans {
            Spans { starts: Vec::new(), ends: Vec::new() }
        }
        pub fn add_start(&mut self, start: u32) {
            self.starts.push(start);
        }
    }
}
"#,
    );
    assert!(!out.contains("this call of `at`"), "{out}");
    assert!(!out.contains("this return from `zeros`"), "{out}");
    assert!(
        out.contains("this return gives `self` back, and the `Spans` it leads to"),
        "{out}"
    );
    assert_eq!(out.matches("this return gives").count(), 1, "{out}");
    assert!(!out.contains("this `Spans { .. }`"), "`new` keeps it: {out}");
}

/// The std table: after a `push` the vector is not empty, so `pop` gives `Some`; a `pop` of a
/// vector nothing shows is not empty is not proved.
#[test]
fn push_and_pop_change_the_length_the_prover_knows() {
    let out = linted(
        r#"
use alloc::vec::Vec;
pub fn last_after_push(mut v: Vec<u32>) -> usize {
    v.push(7);
    if v.len() == 0 {
        blaze::unreachable!()
    }
    v.len() - 1
}
pub fn pop_after_push(mut v: Vec<u32>) -> u32 {
    v.push(7);
    match v.pop() {
        Some(x) => x,
        None => blaze::unreachable!(),
    }
}
pub fn pop_unchecked(mut v: Vec<u32>) -> u32 {
    match v.pop() {
        Some(x) => x,
        None => blaze::unreachable!(),
    }
}
"#,
        "-A clippy::all",
    );
    assert!(out.contains("`pop_unchecked`"), "{out}");
    assert!(!out.contains("`last_after_push`"), "{out}");
    assert!(!out.contains("`pop_after_push`"), "{out}");
}
