// Rung 2: `main` returns a computed exit status, with no `core`: the program declares the lang
// items it needs itself.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for i32 {}

// What a remainder by zero, and a signed remainder of `MIN` by `-1`, call: the checks are in
// every build (the Reference, "Arithmetic and Logical Binary Operators": integer division by
// zero panics, and so does an overflowing `%`; the pinned core's `panic_const`); never return.
#[lang = "panic_const_rem_by_zero"]
fn panic_const_rem_by_zero() -> ! {
    loop {}
}

#[lang = "panic_const_rem_overflow"]
fn panic_const_rem_overflow() -> ! {
    loop {}
}

/// The sum of 1 to `n`.
fn sum_to(n: i32) -> i32 {
    let mut total = 0;
    let mut i = 1;
    while i <= n {
        total += i;
        i = i + 1;
    }
    total
}

fn pick(a: i32, b: i32) -> i32 {
    if a > b { a - b } else { b - a }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    // 1 + 2 + ... + 8 = 36; |36 - 30| = 6; 6 * 7 = 42.
    let s = sum_to(8);
    let d = pick(s, 30);
    d * 7 % 1000
}
