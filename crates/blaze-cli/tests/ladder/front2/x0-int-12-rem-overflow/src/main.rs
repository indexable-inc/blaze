// Own program x0-int-12-rem-overflow (front2), written by the wrong-answers lane (2026-11-16,
// WRONG-NOTES.md "Runner 5", S2): the code builder made `i32::MIN % -1` the machine's 0 (LIR
// `srem`, emit's `msub`), a VALUE where Rust has none.
//
// The Reference, "Overflow" ([expr.operator.int-overflow]): "Using / or %, where the left-hand
// argument is the smallest integer of a signed integer type and the right-hand argument is -1.
// These checks occur even when -C overflow-checks is disabled, for legacy reasons."
// The check calls the pinned core's entry point (library/core/src/panicking.rs, `pub mod
// panic_const`: `panic_const_rem_overflow = "attempt to calculate the remainder with
// overflow"`). This twin declares its own, which raises the panic: the run stops at the raise
// (D91), and `expected` names the fn that raised it. A returned status (0, the machine's
// remainder) is the wrong answer; a hang is `panic_const_rem_by_zero` reached.
//
// The operands come from calls, so nothing about them is known where `%` is built; the casts
// make `MIN` and `-1` without any other operator.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for i32 {}

#[lang = "rem"]
trait Rem<Rhs = Self> {
    type Output;
    fn rem(self, rhs: Rhs) -> Self::Output;
}

impl Rem for i32 {
    type Output = i32;
    fn rem(self, rhs: i32) -> i32 {
        self % rhs
    }
}

unsafe extern "C" {
    fn _Unwind_RaiseException(exception: *mut u8) -> i32;
}

#[lang = "panic_const_rem_overflow"]
fn panic_const_rem_overflow() -> ! {
    unsafe {
        _Unwind_RaiseException(0 as *mut u8);
    }
    loop {}
}

#[lang = "panic_const_rem_by_zero"]
fn panic_const_rem_by_zero() -> ! {
    loop {}
}

fn l() -> i32 {
    0x8000_0000u32 as i32
}

fn r() -> i32 {
    0xFFFF_FFFFu32 as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    l() % r()
}
