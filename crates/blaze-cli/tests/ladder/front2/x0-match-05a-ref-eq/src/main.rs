//! x0-match-05a: the reduction of x0-match-05-nested-enums' wrong answer (2026-11-16: status
//! 3, its guard `w == h` on two `&u32` false for two equal values): `==` and `!=` between
//! two references (no_core; success 42, else the failed check's number). Nothing of the
//! original's enums is needed: the comparison alone is wrong.
//!
//! The Reference, "Operator expressions", "Comparison operators": [expr.cmp.intro]
//! "Comparison operators are also defined both for primitive types and many types in the
//! standard library."; [expr.cmp.place] "Unlike the arithmetic and logical operators above,
//! these operators implicitly take shared borrows of their operands, evaluating them in place
//! expression context: `a == b;` is equivalent to `::std::cmp::PartialEq::eq(&a, &b);`";
//! [expr.cmp.behavior]: `==` is `std::cmp::PartialEq::eq`, `!=` is `std::cmp::PartialEq::ne`.
//! A reference is no primitive the operator is built in for: `a == b` on two `&u32` is the
//! call of `impl PartialEq<&B> for &A` (this program's prelude.rs, as the library's
//! `core/src/cmp.rs`: `PartialEq::eq(*self, *other)`), which compares what they point to.
//!
//! `x` and `y` are two locals holding 4, `z` holds 5: two addresses, one value.
//! 1. `same(&x, &y)`: true. (The wrong builder compares the two addresses: false.)
//! 2. `same(&x, &z)`: false.          3. `differ(&x, &y)` (`!=`): false.
//! 4. `same_t(&x, &y)`, `a == b` on a type parameter at `&u32`: true.
//! 5. `same_t(4, 4)`, the same fn at `u32` (the prelude's `impl PartialEq for u32`): true.
//! 6. the original's shape, a guard: `(a, b) if a == b` on `(&x, &y)`: taken.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

fn same(a: &u32, b: &u32) -> bool {
    a == b
}

fn differ(a: &u32, b: &u32) -> bool {
    a != b
}

fn same_t<T: PartialEq>(a: T, b: T) -> bool {
    a == b
}

fn guarded(t: (&u32, &u32)) -> u32 {
    match t {
        (a, b) if a == b => 1,
        _ => 0,
    }
}

fn check() -> i32 {
    let x = 4u32;
    let y = 4u32;
    let z = 5u32;
    if !same(&x, &y) {
        1
    } else if same(&x, &z) {
        2
    } else if differ(&x, &y) {
        3
    } else if !same_t(&x, &y) {
        4
    } else if !same_t(4u32, 4u32) {
        5
    } else if guarded((&x, &y)) != 1 {
        6
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
