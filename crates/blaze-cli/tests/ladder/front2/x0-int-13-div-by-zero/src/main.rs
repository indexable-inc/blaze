// Own program x0-int-13-div-by-zero (front2), written by the wrong-answers lane (2026-11-16,
// WRONG-NOTES.md "Runner 5", S2): the code builder made `x / 0` a TRAP (LIR
// `Trap::DivByZero`, emit's `brk`), which neither unwinds nor says which check failed.
//
// The Reference, "Arithmetic and Logical Binary Operators": "† For integer types, division by
// zero panics." The check calls the pinned core's entry point (library/core/src/panicking.rs,
// `pub mod panic_const`: `panic_const_div_by_zero = "attempt to divide by zero"`), in every
// build. This twin declares its own, which raises the panic: the run stops at the raise (D91),
// and `expected` names the fn that raised it. A crash (the trap) or a returned status is the
// wrong answer; a hang is `panic_const_div_overflow` reached.
//
// The divisor comes from a call, so nothing about it is known where `/` is built.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u8 {}

#[lang = "div"]
trait Div<Rhs = Self> {
    type Output;
    fn div(self, rhs: Rhs) -> Self::Output;
}

impl Div for u8 {
    type Output = u8;
    fn div(self, rhs: u8) -> u8 {
        self / rhs
    }
}

unsafe extern "C" {
    fn _Unwind_RaiseException(exception: *mut u8) -> i32;
}

#[lang = "panic_const_div_by_zero"]
fn panic_const_div_by_zero() -> ! {
    unsafe {
        _Unwind_RaiseException(0 as *mut u8);
    }
    loop {}
}

#[lang = "panic_const_div_overflow"]
fn panic_const_div_overflow() -> ! {
    loop {}
}

fn zero() -> u8 {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    (42u8 / zero()) as i32
}
