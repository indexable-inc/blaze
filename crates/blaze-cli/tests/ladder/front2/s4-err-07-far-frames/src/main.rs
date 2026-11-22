//! s4-err-07: checked arithmetic and unwinding in a function whose frame is past 32 KiB. `far`
//! makes a thousand calls before its arithmetic, so the arithmetic's values live past the 2048th
//! cell of blaze-emit's frame, out of reach of a load's scaled immediate (pass.rs `FAR`).
//!
//! 2026-11-21, tempo's tests: in such functions every checked `+ - *` panicked "attempt to
//! add/subtract with overflow" where no value overflows, because the far cell's address was made
//! in the register holding the overflow flag. The shapes here are tempo's sites, reduced:
//! serde_json's `check_recursion!` (`remaining_depth -= 1` on a `u8` at 128, then `+= 1`),
//! sha2's `K64[t + 6]`, tempo-primitives' `len - 128` after a `len >= 128` guard and
//! `started_len - buf.len()`, and differences below zero that Rust accepts (`isize`, `i32`,
//! `i8`, `MIN + 1 - 1`). None overflows (the Reference, "Overflow": "When +, * or binary -
//! create a value greater than the maximum value, or less than the minimum value that can be
//! stored"); the true overflow `255u8 + 1` still panics, caught.
//!
//! The same run: a panic unwinds through frames with cleanups (each drops its guard, then
//! `resume`s through `_Unwind_Resume`) to a `catch_unwind`. Before the `resume` call had its
//! own unwind row, the first such `resume` ended the process: "libunwind: _Unwind_Resume -
//! _Unwind_Resume() can't return", SIGABRT.

use std::hint::black_box;
use std::panic;

macro_rules! x10 {
    ($($body:tt)*) => {
        $($body)* $($body)* $($body)* $($body)* $($body)* $($body)* $($body)* $($body)* $($body)* $($body)*
    };
}

struct Guard(&'static str);

impl Drop for Guard {
    fn drop(&mut self) {
        println!("guard {} dropped", self.0);
    }
}

fn far(last: u8) -> u64 {
    let mut acc = 0u64;
    x10! { x10! { x10! { acc = acc.wrapping_add(black_box(1)); } } }
    let mut depth: u8 = black_box(128);
    depth -= 1;
    depth += 1;
    let t = black_box(16usize);
    let len = black_box(200usize);
    let started = black_box(97usize);
    let left = black_box(32usize);
    let diff = black_box(3usize) as isize - black_box(40usize) as isize;
    let small = black_box(-5i32) - black_box(3);
    let byte = black_box(3i8) - black_box(5);
    let min = black_box(i64::MIN) + black_box(1) - black_box(1);
    println!("{acc} {depth} {} {} {} {diff} {small} {byte} {}", t + 6, len - 128, started - left, min == i64::MIN);
    let _g = Guard("far");
    u64::from(black_box(last) + black_box(1))
}

fn inner() {
    let _g = Guard("inner");
    let v: Vec<u8> = Vec::new();
    let _ = black_box(v[black_box(3)]);
}

fn outer() {
    let _g = Guard("outer");
    inner();
}

fn main() {
    panic::set_hook(Box::new(|_| {}));
    println!("{}", far(1));
    match panic::catch_unwind(|| far(255)) {
        Ok(v) => println!("no panic: {v}"),
        Err(p) => println!("caught: {:?}", p.downcast_ref::<&str>()),
    }
    println!("{}", panic::catch_unwind(outer).is_err());
    let _ = panic::take_hook();
    println!("done");
}
