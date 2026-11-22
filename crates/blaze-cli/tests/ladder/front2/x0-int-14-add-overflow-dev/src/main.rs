// Own program x0-int-14-add-overflow-dev (front2), written by the wrong-answers lane (2026-11-16,
// WRONG-NOTES.md "Runner 6", decision D101): `255u8 + 1` built under the dev profile (this
// package's manifest sets nothing, so `overflow-checks` is the dev profile's `true`).
//
// The Reference, "Overflow" ([expr.operator.int-overflow]): "Integer operators will panic when
// they overflow when compiled in debug mode. The -C debug-assertions and -C overflow-checks
// compiler flags can be used to control this more directly." Cargo's profiles: `dev` has
// `overflow-checks = true`. The check calls the pinned core's entry point
// (library/core/src/panicking.rs, `pub mod panic_const`: `panic_const_add_overflow = "attempt
// to add with overflow"`). This twin declares its own, which raises: the run stops at the raise
// (D91) and `expected` names the fn that raised. Status 42 (the wrapped 0, plus 42) is the
// wrong answer: the profile's setting was not taken.
//
// The operands come from calls, so nothing about them is known where `+` is built.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u8 {}
impl Copy for i32 {}

#[lang = "add"]
trait Add<Rhs = Self> {
    type Output;
    fn add(self, rhs: Rhs) -> Self::Output;
}

impl Add for u8 {
    type Output = u8;
    fn add(self, rhs: u8) -> u8 {
        self + rhs
    }
}

impl Add for i32 {
    type Output = i32;
    fn add(self, rhs: i32) -> i32 {
        self + rhs
    }
}

unsafe extern "C" {
    fn _Unwind_RaiseException(exception: *mut u8) -> i32;
}

#[lang = "panic_const_add_overflow"]
fn panic_const_add_overflow() -> ! {
    unsafe {
        _Unwind_RaiseException(0 as *mut u8);
    }
    loop {}
}

fn l() -> u8 {
    255
}

fn r() -> u8 {
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    (l() + r()) as i32 + 42
}
